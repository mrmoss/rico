use anyhow::{Context, Result};
use arboard::Clipboard;
use crossterm::{
    cursor,
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent,
        KeyModifiers,
    },
    execute, queue,
    style::{
        Attribute, Color as CtColor, Print, SetAttribute, SetBackgroundColor,
        SetForegroundColor,
    },
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    cmp::min,
    env,
    fs,
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

const TAB_WIDTH: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Position {
    row: usize,
    col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Find,
    Open,
    Save,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FindField {
    Find,
    Replace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineEnding {
    Lf,
    Crlf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabStyle {
    Tabs,
    Spaces(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CellStyle {
    Normal,
    Selected,
    Match,
    ActiveMatch,
}

impl TabStyle {
    fn description(self) -> String {
        match self {
            Self::Tabs => "TAB".to_string(),
            Self::Spaces(width) => format!("{}SP", width),
        }
    }

    fn insert_string(self) -> String {
        match self {
            Self::Tabs => "\t".to_string(),
            Self::Spaces(width) => " ".repeat(width),
        }
    }
}

fn prev_char_boundary(s: &str, byte_idx: usize) -> usize {
    let safe_idx = min(byte_idx, s.len());
    s[..safe_idx]
        .char_indices()
        .last()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char_boundary(s: &str, byte_idx: usize) -> usize {
    if byte_idx >= s.len() {
        return s.len();
    }
    s[byte_idx..]
        .char_indices()
        .nth(1)
        .map(|(i, _)| byte_idx + i)
        .unwrap_or_else(|| s.len())
}

fn floor_char_boundary(s: &str, byte_idx: usize) -> usize {
    if byte_idx >= s.len() {
        return s.len();
    }
    let mut current = byte_idx;
    while current > 0 && !s.is_char_boundary(current) {
        current -= 1;
    }
    current
}

struct Editor {
    lines: Vec<String>,
    cursor: Position,
    selection_anchor: Option<Position>,

    clipboard_ctx: Option<Clipboard>,
    clipboard_block: Option<String>,
    nano_clipboard: Vec<String>,
    last_action_was_cut: bool,

    has_trailing_newline: bool,

    undo_stack: Vec<(Vec<String>, Position, bool)>,
    redo_stack: Vec<(Vec<String>, Position, bool)>,
    consecutive_typing: usize,

    file_path: Option<PathBuf>,
    line_ending: LineEnding,
    tab_style: TabStyle,

    dirty: bool,
    quit_confirm: bool,

    show_line_numbers: bool,
    word_wrap: bool,

    mode: Mode,
    input_buffer: String,
    save_input_cursor: usize,
    status_message: String,

    find_query: String,
    replace_query: String,
    find_input_cursor: usize,
    replace_input_cursor: usize,
    find_field: FindField,
    find_matches: Vec<Position>,
    find_index: usize,

    terminal_size: (u16, u16),

    scroll_row: usize,
    scroll_v_offset: usize,
    scroll_col: usize,
}

impl Editor {
    fn new(path: Option<PathBuf>, content: String) -> Self {
        let line_ending = detect_line_ending(&content);
        let has_trailing_newline = content.ends_with('\n');
        let lines = parse_lines(&content);
        let tab_style = detect_tab_style(&lines);

        Self {
            lines,
            cursor: Position { row: 0, col: 0 },
            selection_anchor: None,

            clipboard_ctx: Clipboard::new().ok(),
            clipboard_block: None,
            nano_clipboard: Vec::new(),
            last_action_was_cut: false,

            has_trailing_newline,

            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            consecutive_typing: 0,

            file_path: path,
            line_ending,
            tab_style,

            dirty: false,
            quit_confirm: false,

            show_line_numbers: false,
            word_wrap: false,

            mode: Mode::Normal,
            input_buffer: String::new(),
            save_input_cursor: 0,
            status_message: String::new(),

            find_query: String::new(),
            replace_query: String::new(),
            find_input_cursor: 0,
            replace_input_cursor: 0,
            find_field: FindField::Find,
            find_matches: Vec::new(),
            find_index: 0,

            terminal_size: terminal::size().unwrap_or((80, 24)),

            scroll_row: 0,
            scroll_v_offset: 0,
            scroll_col: 0,
        }
    }

    fn run(&mut self) -> Result<()> {
        terminal::enable_raw_mode()?;

        let mut stdout = io::stdout();

        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            Clear(ClearType::All),
            cursor::Hide
        )?;

        let result = self.run_loop(&mut stdout);

        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            cursor::Show
        );

        let _ = terminal::disable_raw_mode();

        result
    }

    fn run_loop(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        let mut needs_redraw = true;

        loop {
            let current_size = terminal::size().unwrap_or((80, 24));
            if current_size != self.terminal_size {
                self.terminal_size = current_size;
                needs_redraw = true;
            }

            if needs_redraw {
                self.draw(stdout)?;
                needs_redraw = false;
            }

            if event::poll(Duration::from_millis(50))? {
                needs_redraw = true;

                match event::read()? {
                    Event::Key(key) => {
                        let should_quit = match self.mode {
                            Mode::Normal => self.handle_normal_key(key)?,
                            Mode::Find => {
                                self.handle_find_key(key)?;
                                false
                            }
                            Mode::Open => {
                                self.handle_open_key(key)?;
                                false
                            }
                            Mode::Save => {
                                self.handle_save_key(key)?;
                                false
                            }
                        };

                        if should_quit {
                            break;
                        }
                    }

                    Event::Paste(text) => {
                        if self.mode == Mode::Normal {
                            self.last_action_was_cut = false;
                            self.snapshot();

                            if let Some(sel) = self.selection_anchor {
                                self.delete_selection(sel, self.cursor);
                                self.selection_anchor = None;
                            }

                            self.insert_text(&text);
                            self.status_message =
                                format!("Pasted {} characters.", text.chars().count());
                        }
                    }

                    Event::Resize(_, _) => {
                        self.terminal_size = terminal::size().unwrap_or((80, 24));
                    }

                    _ => {}
                }
            }
        }

        Ok(())
    }

    fn snapshot(&mut self) {
        self.undo_stack
            .push((self.lines.clone(), self.cursor, self.has_trailing_newline));

        if self.undo_stack.len() > 100 {
            self.undo_stack.remove(0);
        }

        self.redo_stack.clear();
        self.consecutive_typing = 0;
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Result<bool> {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if ctrl && shift && key.code == KeyCode::Char('c') {
            return Ok(false);
        }

        if ctrl && shift && key.code == KeyCode::Char('v') {
            return Ok(false);
        }

        let is_ctrl_k = ctrl && key.code == KeyCode::Char('k');

        if !is_ctrl_k {
            self.last_action_was_cut = false;
        }

        let is_movement = matches!(
            key.code,
            KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
        );

        if !shift && is_movement {
            self.selection_anchor = None;
        }

        if ctrl && key.code == KeyCode::Char('q') {
            if self.dirty {
                if !self.quit_confirm {
                    self.quit_confirm = true;
                    self.status_message =
                        "UNSAVED CHANGES! Press Ctrl+Q again to force quit.".to_string();
                    return Ok(false);
                }

                return Ok(true);
            }

            return Ok(true);
        }

        if self.quit_confirm {
            self.quit_confirm = false;
            self.status_message.clear();
        }

        match (ctrl, key.code) {
            (true, KeyCode::Char('s')) => {
                self.mode = Mode::Save;
                let initial_path = self.file_path
                    .as_ref()
                    .map(|p| {
                        if p.is_absolute() {
                            p.display().to_string()
                        } else {
                            env::current_dir()
                                .map(|d| d.join(p).display().to_string())
                                .unwrap_or_else(|_| p.display().to_string())
                        }
                    })
                    .unwrap_or_default();
                self.input_buffer = initial_path;
                self.save_input_cursor = self.input_buffer.chars().count();
                self.status_message.clear();
            }

            (true, KeyCode::Char('o')) => {
                self.mode = Mode::Open;
                self.input_buffer.clear();
                self.status_message.clear();
            }

            (true, KeyCode::Char('z')) => {
                if let Some((lines, cursor, htn)) = self.undo_stack.pop() {
                    self.redo_stack.push((
                        self.lines.clone(),
                        self.cursor,
                        self.has_trailing_newline,
                    ));

                    self.lines = lines;
                    self.cursor = cursor;
                    self.has_trailing_newline = htn;
                    self.dirty = true;
                    self.selection_anchor = None;

                    self.status_message = "Undo".to_string();
                } else {
                    self.status_message = "Nothing to undo.".to_string();
                }
            }

            (true, KeyCode::Char('y')) => {
                if let Some((lines, cursor, htn)) = self.redo_stack.pop() {
                    self.undo_stack.push((
                        self.lines.clone(),
                        self.cursor,
                        self.has_trailing_newline,
                    ));

                    self.lines = lines;
                    self.cursor = cursor;
                    self.has_trailing_newline = htn;
                    self.dirty = true;
                    self.selection_anchor = None;

                    self.status_message = "Redo".to_string();
                } else {
                    self.status_message = "Nothing to redo.".to_string();
                }
            }

            (true, KeyCode::Char('w')) => {
                self.word_wrap = !self.word_wrap;
                if self.word_wrap {
                    self.scroll_col = 0;
                }
                self.status_message = format!("Word Wrap {}", if self.word_wrap { "ON" } else { "OFF" });
            }

            (true, KeyCode::Char('x')) => {
                if let Some(sel) = self.selection_anchor {
                    self.snapshot();
                    let text = self.extract_selection(sel, self.cursor);
                    if let Some(ctx) = &mut self.clipboard_ctx {
                        let _ = ctx.set_text(text.clone());
                    }
                    self.clipboard_block = Some(text);
                    self.delete_selection(sel, self.cursor);
                    self.selection_anchor = None;
                    self.dirty = true;
                    self.status_message = "Block cut to clipboard.".to_string();
                }
            }

            (true, KeyCode::Char('c')) => {
                if let Some(sel) = self.selection_anchor {
                    let text = self.extract_selection(sel, self.cursor);
                    if let Some(ctx) = &mut self.clipboard_ctx {
                        let _ = ctx.set_text(text.clone());
                    }
                    self.clipboard_block = Some(text);
                    self.status_message = "Block copied to clipboard.".to_string();
                }
            }

            (true, KeyCode::Char('v')) => {
                let text = self.clipboard_ctx.as_mut()
                    .and_then(|ctx| ctx.get_text().ok())
                    .or_else(|| self.clipboard_block.clone());

                if let Some(block) = text {
                    self.snapshot();
                    if let Some(sel) = self.selection_anchor {
                        self.delete_selection(sel, self.cursor);
                        self.selection_anchor = None;
                    }
                    self.insert_text(&block);
                    self.status_message = "Pasted from clipboard.".to_string();
                }
            }

            (true, KeyCode::Char('k')) => {
                self.snapshot();
                if self.cursor.row < self.lines.len() {
                    let cut = self.lines.remove(self.cursor.row);

                    if !self.last_action_was_cut {
                        self.nano_clipboard.clear();
                    }

                    self.nano_clipboard.push(cut);
                    self.last_action_was_cut = true;

                    if self.lines.is_empty() {
                        self.lines.push(String::new());
                        self.has_trailing_newline = false;
                    }

                    self.cursor.row = min(self.cursor.row, self.max_navigable_row());
                    self.cursor.col = min(self.cursor.col, self.lines[self.cursor.row].len());
                    self.cursor.col = floor_char_boundary(&self.lines[self.cursor.row], self.cursor.col);

                    self.dirty = true;
                    self.status_message =
                        format!("Cut line ({})", self.nano_clipboard.len());
                }
            }

            (true, KeyCode::Char('u')) => {
                if !self.nano_clipboard.is_empty() {
                    self.snapshot();
                    let clipboard_lines = self.nano_clipboard.clone();
                    for (i, line) in clipboard_lines.into_iter().enumerate() {
                        self.lines.insert(self.cursor.row + i, line);
                    }
                    self.cursor.row += self.nano_clipboard.len();
                    self.cursor.row = min(self.cursor.row, self.max_navigable_row());
                    self.cursor.col = 0;
                    self.dirty = true;
                    self.status_message = "Pasted cut lines.".to_string();
                }
            }

            (true, KeyCode::Char('n')) => {
                self.show_line_numbers = !self.show_line_numbers;
                self.status_message = format!(
                    "Line numbers {}",
                    if self.show_line_numbers { "ON" } else { "OFF" }
                );
            }

            (true, KeyCode::Char('a')) => {
                self.selection_anchor = Some(Position { row: 0, col: 0 });
                let target_row = self.max_navigable_row();
                self.cursor = Position {
                    row: target_row,
                    col: self.lines[target_row].len(),
                };
            }

            (true, KeyCode::Char('f')) => {
                self.selection_anchor = None;
                self.mode = Mode::Find;
                self.find_field = FindField::Find;
                self.find_input_cursor = self.find_query.chars().count();
                self.replace_input_cursor = self.replace_query.chars().count();
                self.status_message.clear();
            }

            (_, KeyCode::Home) => {
                if shift && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor);
                }
                if ctrl {
                    self.cursor.row = 0;
                }
                self.cursor.col = 0;
            }

            (_, KeyCode::End) => {
                if shift && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor);
                }
                if ctrl {
                    self.cursor.row = self.max_navigable_row();
                }
                self.cursor.col = self.lines[self.cursor.row].len();
            }

            (_, KeyCode::PageUp) => {
                if shift && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor);
                }
                let page_size = self.terminal_size.1.saturating_sub(4).max(1) as usize;
                if self.cursor.row == 0 {
                    self.cursor.col = 0;
                } else {
                    self.cursor.row = self.cursor.row.saturating_sub(page_size);
                    self.cursor.col = min(self.cursor.col, self.lines[self.cursor.row].len());
                    self.cursor.col = floor_char_boundary(&self.lines[self.cursor.row], self.cursor.col);
                }
            }

            (_, KeyCode::PageDown) => {
                if shift && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor);
                }
                let page_size = self.terminal_size.1.saturating_sub(4).max(1) as usize;
                let last_row = self.max_navigable_row();
                if self.cursor.row == last_row {
                    self.cursor.col = self.lines[last_row].len();
                } else {
                    self.cursor.row = min(last_row, self.cursor.row + page_size);
                    self.cursor.col = min(self.cursor.col, self.lines[self.cursor.row].len());
                    self.cursor.col = floor_char_boundary(&self.lines[self.cursor.row], self.cursor.col);
                }
            }

            (_, KeyCode::Left) => {
                if shift && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor);
                }
                if self.cursor.col > 0 {
                    self.cursor.col = prev_char_boundary(&self.lines[self.cursor.row], self.cursor.col);
                } else if self.cursor.row > 0 {
                    self.cursor.row -= 1;
                    self.cursor.col = self.lines[self.cursor.row].len();
                }
            }

            (_, KeyCode::Right) => {
                if shift && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor);
                }
                let line_len = self.lines[self.cursor.row].len();
                if self.cursor.col < line_len {
                    self.cursor.col = next_char_boundary(&self.lines[self.cursor.row], self.cursor.col);
                } else if self.cursor.row < self.max_navigable_row() {
                    self.cursor.row += 1;
                    self.cursor.col = 0;
                }
            }

            (_, KeyCode::Up) => {
                if shift && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor);
                }
                if self.cursor.row > 0 {
                    self.cursor.row -= 1;
                    self.cursor.col = min(self.cursor.col, self.lines[self.cursor.row].len());
                    self.cursor.col = floor_char_boundary(&self.lines[self.cursor.row], self.cursor.col);
                } else {
                    self.cursor.col = 0;
                }
            }

            (_, KeyCode::Down) => {
                if shift && self.selection_anchor.is_none() {
                    self.selection_anchor = Some(self.cursor);
                }
                if self.cursor.row < self.max_navigable_row() {
                    self.cursor.row += 1;
                    self.cursor.col = min(self.cursor.col, self.lines[self.cursor.row].len());
                    self.cursor.col = floor_char_boundary(&self.lines[self.cursor.row], self.cursor.col);
                } else {
                    self.cursor.col = self.lines[self.cursor.row].len();
                }
            }

            (_, KeyCode::Backspace) => {
                self.snapshot();
                if let Some(sel) = self.selection_anchor {
                    self.delete_selection(sel, self.cursor);
                    self.selection_anchor = None;
                    self.dirty = true;
                } else if self.cursor.col > 0 {
                    let prev_col = prev_char_boundary(&self.lines[self.cursor.row], self.cursor.col);
                    self.lines[self.cursor.row].drain(prev_col..self.cursor.col);
                    self.cursor.col = prev_col;
                    self.dirty = true;
                } else if self.cursor.row > 0 {
                    let current_line = self.lines.remove(self.cursor.row);
                    self.cursor.row -= 1;
                    self.cursor.col = self.lines[self.cursor.row].len();
                    self.lines[self.cursor.row].push_str(&current_line);
                    self.dirty = true;
                    self.update_trailing_newline();
                }
            }

            (_, KeyCode::Delete) => {
                self.snapshot();
                if let Some(sel) = self.selection_anchor {
                    self.delete_selection(sel, self.cursor);
                    self.selection_anchor = None;
                    self.dirty = true;
                } else if self.cursor.col < self.lines[self.cursor.row].len() {
                    let next_col = next_char_boundary(&self.lines[self.cursor.row], self.cursor.col);
                    self.lines[self.cursor.row].drain(self.cursor.col..next_col);
                    self.dirty = true;
                } else if self.cursor.row < self.lines.len() - 1 {
                    let next_line = self.lines.remove(self.cursor.row + 1);
                    self.lines[self.cursor.row].push_str(&next_line);
                    self.dirty = true;
                    self.update_trailing_newline();
                }
            }

            (_, KeyCode::Enter) => {
                self.snapshot();
                if let Some(sel) = self.selection_anchor {
                    self.delete_selection(sel, self.cursor);
                    self.selection_anchor = None;
                }
                let current_line = &mut self.lines[self.cursor.row];
                let remainder = current_line.split_off(self.cursor.col);
                self.cursor.row += 1;
                self.lines.insert(self.cursor.row, remainder);
                self.cursor.col = 0;
                self.has_trailing_newline =
                    self.cursor.row == self.lines.len() - 1;
                self.dirty = true;
            }

            (_, KeyCode::Tab) => {
                self.snapshot();
                if let Some(sel) = self.selection_anchor {
                    self.delete_selection(sel, self.cursor);
                    self.selection_anchor = None;
                }
                let tab = self.tab_style.insert_string();
                self.lines[self.cursor.row].insert_str(self.cursor.col, &tab);
                self.cursor.col += tab.len();
                self.dirty = true;
                self.status_message = format!("Inserted {}", self.tab_style.description());
            }

            (_, KeyCode::Char(c)) => {
                if !ctrl {
                    if let Some(sel) = self.selection_anchor {
                        self.snapshot();
                        self.delete_selection(sel, self.cursor);
                        self.selection_anchor = None;
                        self.consecutive_typing = 0;
                    } else {
                        if self.consecutive_typing % 20 == 0 {
                            self.undo_stack.push((
                                self.lines.clone(),
                                self.cursor,
                                self.has_trailing_newline,
                            ));
                            self.redo_stack.clear();
                        }
                        self.consecutive_typing += 1;
                    }
                    self.lines[self.cursor.row].insert(self.cursor.col, c);
                    self.cursor.col += c.len_utf8();
                    self.dirty = true;
                }
            }

            _ => {}
        }

        if self.dirty {
            self.refresh_tab_style_if_needed();
        }

        Ok(false)
    }

    fn handle_find_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') if ctrl || matches!(key.code, KeyCode::Esc) => {
                self.mode = Mode::Normal;
                self.status_message.clear();
            }
            KeyCode::Tab | KeyCode::Up | KeyCode::Down => {
                self.find_field = match self.find_field {
                    FindField::Find => FindField::Replace,
                    FindField::Replace => FindField::Find,
                };
            }
            KeyCode::Left => {
                match self.find_field {
                    FindField::Find => {
                        self.find_input_cursor = self.find_input_cursor.saturating_sub(1);
                    }
                    FindField::Replace => {
                        self.replace_input_cursor = self.replace_input_cursor.saturating_sub(1);
                    }
                }
            }
            KeyCode::Right => {
                match self.find_field {
                    FindField::Find => {
                        let len = self.find_query.chars().count();
                        if self.find_input_cursor < len {
                            self.find_input_cursor += 1;
                        }
                    }
                    FindField::Replace => {
                        let len = self.replace_query.chars().count();
                        if self.replace_input_cursor < len {
                            self.replace_input_cursor += 1;
                        }
                    }
                }
            }
            KeyCode::Home => match self.find_field {
                FindField::Find => self.find_input_cursor = 0,
                FindField::Replace => self.replace_input_cursor = 0,
            },
            KeyCode::End => match self.find_field {
                FindField::Find => self.find_input_cursor = self.find_query.chars().count(),
                FindField::Replace => self.replace_input_cursor = self.replace_query.chars().count(),
            },
            KeyCode::Enter => {
                match self.find_field {
                    FindField::Find => self.find_next(),
                    FindField::Replace => self.replace_current_match()?,
                }
            }
            KeyCode::Char('a') if ctrl => {
                self.replace_all()?;
            }
            KeyCode::Char('n') if ctrl => {
                self.find_next();
            }
            KeyCode::Char('p') if ctrl => {
                self.find_prev();
            }
            KeyCode::Char('r') if ctrl => {
                self.replace_current_match()?;
            }
            KeyCode::F(3) => {
                if shift {
                    self.find_prev();
                } else {
                    self.find_next();
                }
            }
            KeyCode::Backspace => match self.find_field {
                FindField::Find => {
                    let (new_s, new_c) = remove_char_before(&self.find_query, self.find_input_cursor);
                    self.find_query = new_s;
                    self.find_input_cursor = new_c;
                    self.execute_find(&self.find_query.clone());
                }
                FindField::Replace => {
                    let (new_s, new_c) = remove_char_before(&self.replace_query, self.replace_input_cursor);
                    self.replace_query = new_s;
                    self.replace_input_cursor = new_c;
                }
            },
            KeyCode::Delete => match self.find_field {
                FindField::Find => {
                    let (new_s, new_c) = remove_char_at(&self.find_query, self.find_input_cursor);
                    self.find_query = new_s;
                    self.find_input_cursor = new_c;
                    self.execute_find(&self.find_query.clone());
                }
                FindField::Replace => {
                    let (new_s, new_c) = remove_char_at(&self.replace_query, self.replace_input_cursor);
                    self.replace_query = new_s;
                    self.replace_input_cursor = new_c;
                }
            },
            KeyCode::Char(c) => {
                if !ctrl {
                    match self.find_field {
                        FindField::Find => {
                            let (new_s, new_c) = insert_char_at(&self.find_query, self.find_input_cursor, c);
                            self.find_query = new_s;
                            self.find_input_cursor = new_c;
                            self.execute_find(&self.find_query.clone());
                        }
                        FindField::Replace => {
                            let (new_s, new_c) = insert_char_at(&self.replace_query, self.replace_input_cursor, c);
                            self.replace_query = new_s;
                            self.replace_input_cursor = new_c;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_open_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status_message.clear();
            }
            KeyCode::Enter => {
                let path = PathBuf::from(&self.input_buffer);
                match fs::read_to_string(&path) {
                    Ok(content) => {
                        self.line_ending = detect_line_ending(&content);
                        self.has_trailing_newline = content.ends_with('\n');
                        self.lines = parse_lines(&content);
                        self.tab_style = detect_tab_style(&self.lines);
                        self.file_path = Some(path);

                        self.cursor = Position { row: 0, col: 0 };
                        self.selection_anchor = None;
                        self.undo_stack.clear();
                        self.redo_stack.clear();
                        self.dirty = false;
                        self.quit_confirm = false;

                        self.status_message = format!(
                            "Loaded {}",
                            self.file_path.as_ref().unwrap().display()
                        );
                    }
                    Err(e) => {
                        self.status_message = format!("Error opening file: {}", e);
                    }
                }
                self.mode = Mode::Normal;
            }
            KeyCode::Backspace => {
                self.input_buffer.pop();
            }
            KeyCode::Char(c) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                if !ctrl {
                    self.input_buffer.push(c);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_save_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') if ctrl || matches!(key.code, KeyCode::Esc) => {
                self.mode = Mode::Normal;
                self.status_message.clear();
            }
            KeyCode::Enter => {
                let path_to_save = if self.input_buffer.trim().is_empty() {
                    self.file_path.clone()
                } else {
                    Some(PathBuf::from(self.input_buffer.trim()))
                };

                if let Some(path) = path_to_save {
                    self.save_to_path(path)?;
                } else {
                    self.status_message = "Error: No file name specified.".to_string();
                }
                self.mode = Mode::Normal;
            }
            KeyCode::Left => {
                self.save_input_cursor = self.save_input_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let len = self.input_buffer.chars().count();
                if self.save_input_cursor < len {
                    self.save_input_cursor += 1;
                }
            }
            KeyCode::Home => {
                self.save_input_cursor = 0;
            }
            KeyCode::End => {
                self.save_input_cursor = self.input_buffer.chars().count();
            }
            KeyCode::Backspace => {
                let (new_s, new_c) = remove_char_before(&self.input_buffer, self.save_input_cursor);
                self.input_buffer = new_s;
                self.save_input_cursor = new_c;
            }
            KeyCode::Delete => {
                let (new_s, new_c) = remove_char_at(&self.input_buffer, self.save_input_cursor);
                self.input_buffer = new_s;
                self.save_input_cursor = new_c;
            }
            KeyCode::Char(c) => {
                if !ctrl {
                    let (new_s, new_c) = insert_char_at(&self.input_buffer, self.save_input_cursor, c);
                    self.input_buffer = new_s;
                    self.save_input_cursor = new_c;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn save_to_path(&mut self, path: PathBuf) -> Result<()> {
        let join_str = match self.line_ending {
            LineEnding::Crlf => "\r\n",
            LineEnding::Lf => "\n",
        };

        let content = self.lines.join(join_str);

        match fs::write(&path, content) {
            Ok(_) => {
                self.file_path = Some(path);
                self.dirty = false;
                self.quit_confirm = false;
                self.status_message = format!("Wrote {} lines", self.lines.len());
                Ok(())
            }
            Err(e) => {
                self.status_message = format!("Error writing file: {}", e);
                Err(e).context("failed to write file")
            }
        }
    }

    fn execute_find(&mut self, query: &str) {
        self.find_query = query.to_string();
        self.find_matches.clear();

        if query.is_empty() {
            return;
        }

        let decoded_query = decode_escapes(query);
        if decoded_query.is_empty() {
            return;
        }

        let content = self.lines.join("\n");
        let mut start = 0;
        while start < content.len() {
            let Some(idx) = content[start..].find(&decoded_query) else {
                break;
            };
            let match_offset = start + idx;
            let pos = offset_to_position(&content, match_offset);
            self.find_matches.push(pos);
            start = match_offset + decoded_query.len().max(1);
        }

        if !self.find_matches.is_empty() {
            self.cursor = self.find_matches[0];
            self.find_index = 0;
        }
    }

    fn find_next(&mut self) {
        let query = self.find_query.clone();
        if !query.is_empty() {
            if self.find_matches.is_empty() || self.find_query != query {
                self.execute_find(&query);
            } else if !self.find_matches.is_empty() {
                self.find_index = (self.find_index + 1) % self.find_matches.len();
                self.cursor = self.find_matches[self.find_index];
            }
        }
    }

    fn find_prev(&mut self) {
        let query = self.find_query.clone();
        if !query.is_empty() {
            if self.find_matches.is_empty() || self.find_query != query {
                self.execute_find(&query);
            } else if !self.find_matches.is_empty() {
                if self.find_index == 0 {
                    self.find_index = self.find_matches.len() - 1;
                } else {
                    self.find_index -= 1;
                }
                self.cursor = self.find_matches[self.find_index];
            }
        }
    }

    fn replace_current_match(&mut self) -> Result<()> {
        if self.find_matches.is_empty() {
            self.status_message = "No match to replace.".to_string();
            return Ok(());
        }

        let pos = self.find_matches[self.find_index];
        let decoded_find = decode_escapes(&self.find_query);
        let decoded_replace = decode_escapes(&self.replace_query);

        if decoded_find.is_empty() {
            return Ok(());
        }

        let content = self.lines.join("\n");
        let mut current_row = 0;
        let mut line_start_byte = 0;
        let mut byte_offset = None;

        for (i, b) in content.bytes().enumerate() {
            if current_row == pos.row && (i - line_start_byte) == pos.col {
                byte_offset = Some(i);
                break;
            }
            if b == b'\n' {
                current_row += 1;
                line_start_byte = i + 1;
            }
        }
        if current_row == pos.row && (content.len() - line_start_byte) == pos.col && byte_offset.is_none() {
            byte_offset = Some(content.len());
        }

        if let Some(offset) = byte_offset {
            if offset + decoded_find.len() <= content.len() {
                self.snapshot();
                let mut new_content = content;
                new_content.replace_range(offset..offset + decoded_find.len(), &decoded_replace);

                self.has_trailing_newline = new_content.ends_with('\n');
                self.lines = parse_lines(&new_content);
                self.dirty = true;
                self.update_trailing_newline();

                let old_index = self.find_index;
                self.execute_find(&self.find_query.clone());
                if !self.find_matches.is_empty() {
                    self.find_index = old_index.min(self.find_matches.len().saturating_sub(1));
                    self.cursor = self.find_matches[self.find_index];
                }
                self.status_message = "Replaced match.".to_string();
            }
        }
        Ok(())
    }

    fn replace_all(&mut self) -> Result<()> {
        let decoded_find = decode_escapes(&self.find_query);
        let decoded_replace = decode_escapes(&self.replace_query);

        if decoded_find.is_empty() {
            return Ok(());
        }

        let content = self.lines.join("\n");
        if content.contains(&decoded_find) {
            self.snapshot();
            let new_content = content.replace(&decoded_find, &decoded_replace);
            self.has_trailing_newline = new_content.ends_with('\n');
            self.lines = parse_lines(&new_content);
            self.dirty = true;
            self.update_trailing_newline();
            self.execute_find(&self.find_query.clone());
            self.status_message = format!("Replaced all occurrences of '{}'.", self.find_query);
        } else {
            self.status_message = "No matches found to replace.".to_string();
        }
        Ok(())
    }

    fn extract_selection(&self, start: Position, end: Position) -> String {
        let (s, e) = self.normalized_selection(start, end);

        if s.row >= self.lines.len() || e.row >= self.lines.len() {
            return String::new();
        }

        if s.row == e.row {
            let line = &self.lines[s.row];
            let start_col = floor_char_boundary(line, min(s.col, line.len()));
            let end_col = floor_char_boundary(line, min(e.col, line.len()));
            return line[start_col..end_col].to_string();
        }

        let mut result = String::new();
        let s_line = &self.lines[s.row];
        let s_col = floor_char_boundary(s_line, min(s.col, s_line.len()));
        result.push_str(&s_line[s_col..]);
        result.push('\n');

        for r in (s.row + 1)..e.row {
            result.push_str(&self.lines[r]);
            result.push('\n');
        }

        let e_line = &self.lines[e.row];
        let e_col = floor_char_boundary(e_line, min(e.col, e_line.len()));
        result.push_str(&e_line[..e_col]);

        result
    }

    fn delete_selection(&mut self, start: Position, end: Position) {
        let (s, e) = self.normalized_selection(start, end);

        if s.row >= self.lines.len() || e.row >= self.lines.len() {
            return;
        }

        if s.row == e.row {
            let line = &mut self.lines[s.row];
            let start_col = floor_char_boundary(line, min(s.col, line.len()));
            let end_col = floor_char_boundary(line, min(e.col, line.len()));
            if start_col < end_col {
                line.drain(start_col..end_col);
            }
            self.cursor = Position { row: s.row, col: start_col };
            return;
        }

        let e_line = &self.lines[e.row];
        let e_col = floor_char_boundary(e_line, min(e.col, e_line.len()));
        let end_tail = if e_col < e_line.len() {
            e_line[e_col..].to_string()
        } else {
            String::new()
        };

        let s_line = &mut self.lines[s.row];
        let s_col = floor_char_boundary(s_line, min(s.col, s_line.len()));
        s_line.truncate(s_col);
        s_line.push_str(&end_tail);

        for _ in 0..(e.row - s.row) {
            if s.row + 1 < self.lines.len() {
                self.lines.remove(s.row + 1);
            }
        }

        if self.lines.is_empty() {
            self.lines.push(String::new());
        }

        self.cursor = Position { row: s.row, col: s_col };
        self.update_trailing_newline();
    }

    fn normalized_selection(
        &self,
        start: Position,
        end: Position,
    ) -> (Position, Position) {
        if start.row < end.row
            || (start.row == end.row && start.col <= end.col)
        {
            (start, end)
        } else {
            (end, start)
        }
    }

    fn insert_text(&mut self, text: &str) {
        let parts: Vec<&str> = text
            .split('\n')
            .map(|s| s.trim_end_matches('\r'))
            .collect();

        for (i, part) in parts.iter().enumerate() {
            if i == 0 {
                self.lines[self.cursor.row]
                    .insert_str(self.cursor.col, part);
                self.cursor.col += part.len();
            } else {
                let remainder =
                    self.lines[self.cursor.row].split_off(self.cursor.col);
                self.cursor.row += 1;
                self.lines.insert(self.cursor.row, remainder);
                self.lines[self.cursor.row].insert_str(0, part);
                self.cursor.col = part.len();
            }
        }

        self.update_trailing_newline();
        self.dirty = true;
    }

    fn update_trailing_newline(&mut self) {
        self.has_trailing_newline =
            self.lines.len() > 1 && self.lines.last().is_some_and(String::is_empty);
    }

    fn max_navigable_row(&self) -> usize {
        let last = self.lines.len().saturating_sub(1);
        if last > 0 && self.lines[last].is_empty() {
            last - 1
        } else {
            last
        }
    }

    fn refresh_tab_style_if_needed(&mut self) {
        let detected = detect_tab_style(&self.lines);
        match detected {
            TabStyle::Spaces(_) => {
                self.tab_style = detected;
            }
            TabStyle::Tabs => {
                if self.lines.iter().any(|line| line.starts_with('\t')) {
                    self.tab_style = TabStyle::Tabs;
                }
            }
        }
    }

    fn draw(&mut self, stdout: &mut io::Stdout) -> Result<()> {
        let width = self.terminal_size.0 as usize;
        let height = self.terminal_size.1 as usize;

        if width < 10 || height < 6 {
            return Ok(());
        }

        queue!(stdout, cursor::Hide)?;

        let bottom_rows = 3;
        // Row 0 is the top bar, so the content area starts at row 1.
        // Leave all bottom_rows exclusively for the status/instruction bars.
        let content_height = height.saturating_sub(bottom_rows + 1);

        let max_line_digits = self.lines.len().to_string().len();
        let gutter_width = if self.show_line_numbers {
            max_line_digits + 2
        } else {
            0
        };

        let content_width = width.saturating_sub(gutter_width);

        let current_line = self
            .lines
            .get(self.cursor.row)
            .map(String::as_str)
            .unwrap_or("");

        let logical_col = min(self.cursor.col, current_line.len());
        let visual_col = visual_width_until(current_line, logical_col);

        if !self.word_wrap {
            if visual_col < self.scroll_col {
                self.scroll_col = visual_col;
            } else if visual_col >= self.scroll_col + content_width && content_width > 0 {
                self.scroll_col = visual_col - content_width + 1;
            }
        } else {
            self.scroll_col = 0;
        }

        if self.cursor.row < self.scroll_row {
            self.scroll_row = self.cursor.row;
            self.scroll_v_offset = 0;
        }

        let mut cursor_v_idx = 0;
        for r in self.scroll_row..=self.cursor.row {
            let chunks = wrap_line(&self.lines[r], content_width, self.word_wrap);
            if r == self.cursor.row {
                for (i, chunk) in chunks.iter().enumerate() {
                    let is_last = chunk.1 == self.lines[r].len();
                    if self.cursor.col >= chunk.0 && (self.cursor.col < chunk.1 || (self.cursor.col == chunk.1 && is_last)) {
                        cursor_v_idx += i;
                        break;
                    }
                }
            } else {
                cursor_v_idx += chunks.len().max(1);
            }
        }

        let margin = if matches!(self.mode, Mode::Find | Mode::Save) { 3 } else { 0 };
        if cursor_v_idx < self.scroll_v_offset {
            self.scroll_v_offset = cursor_v_idx.saturating_sub(margin);
        } else if cursor_v_idx >= self.scroll_v_offset + content_height.saturating_sub(margin) && content_height > margin {
            self.scroll_v_offset = cursor_v_idx + margin + 1 - content_height;
        }

        while self.scroll_row < self.lines.len() {
            let chunks = wrap_line(&self.lines[self.scroll_row], content_width, self.word_wrap).len().max(1);
            if self.scroll_v_offset >= chunks && chunks > 0 {
                self.scroll_v_offset -= chunks;
                self.scroll_row += 1;
            } else {
                break;
            }
        }

        let mut visual_lines_to_draw = Vec::new();
        let mut current_row = self.scroll_row;
        let mut current_v_offset = self.scroll_v_offset;

        while visual_lines_to_draw.len() < content_height && current_row < self.lines.len() {
            let chunks = wrap_line(&self.lines[current_row], content_width, self.word_wrap);
            for (i, chunk) in chunks.into_iter().enumerate().skip(current_v_offset) {
                if visual_lines_to_draw.len() >= content_height {
                    break;
                }
                visual_lines_to_draw.push((current_row, chunk.0, chunk.1, i == 0));
            }
            current_v_offset = 0;
            current_row += 1;
        }

        self.draw_top_bar(stdout, width)?;

        let selection = self
            .selection_anchor
            .map(|anchor| self.normalized_selection(anchor, self.cursor));

        let decoded_query = decode_escapes(&self.find_query);

        for i in 0..content_height {
            queue!(stdout, cursor::MoveTo(0, (i + 1) as u16))?;

            if i < visual_lines_to_draw.len() {
                let (r, s, e, is_first) = visual_lines_to_draw[i];
                self.draw_visual_line(
                    stdout, r, s, e, width, content_width, gutter_width, self.scroll_col, selection, is_first, &decoded_query
                )?;
            } else {
                queue!(
                    stdout,
                    SetForegroundColor(CtColor::DarkGrey),
                    Print('~'),
                    SetForegroundColor(CtColor::Reset),
                    Clear(ClearType::UntilNewLine)
                )?;
            }
        }

        if self.mode == Mode::Find {
            self.draw_find_replace_bars(stdout, width, height)?;
        } else if self.mode == Mode::Save {
            self.draw_save_bar(stdout, width, height)?;
        } else if self.quit_confirm {
            self.draw_quit_warning_bar(stdout, width, height)?;
        } else {
            self.draw_status_bar(stdout, width)?;
            self.draw_message_bar(stdout, width)?;
        }

        let mut cursor_visible = false;
        let mut target_x = 0;
        let mut target_y = 0;

        if self.mode == Mode::Normal && !self.quit_confirm && self.selection_anchor.is_none() {
            let mut cursor_should_be_visible = false;
            let mut cursor_x = gutter_width;
            let mut cursor_y_idx = 1;

            for (i, &(r, s, e, _)) in visual_lines_to_draw.iter().enumerate() {
                if r == self.cursor.row {
                    let is_last = e == self.lines[r].len();
                    if self.cursor.col >= s && (self.cursor.col < e || (self.cursor.col == e && is_last)) {
                        cursor_should_be_visible = true;
                        cursor_y_idx = i + 1;

                        let chunk_before_cursor = &self.lines[r][s..self.cursor.col];
                        let v_col = visual_width_until(chunk_before_cursor, chunk_before_cursor.len());
                        cursor_x = gutter_width + v_col.saturating_sub(self.scroll_col);
                        break;
                    }
                }
            }

            if cursor_should_be_visible
                && cursor_y_idx < height.saturating_sub(3)
                && cursor_x <= width
            {
                cursor_visible = true;
                target_x = cursor_x as u16;
                target_y = cursor_y_idx as u16;
            }
        } else if self.mode == Mode::Find {
            let (cursor_x, cursor_y) = match self.find_field {
                FindField::Find => (7 + self.find_input_cursor, (height - 3) as u16),
                FindField::Replace => (10 + self.replace_input_cursor, (height - 2) as u16),
            };
            cursor_visible = true;
            target_x = cursor_x.min(width.saturating_sub(1)) as u16;
            target_y = cursor_y;
        } else if self.mode == Mode::Save {
            let prompt = " File Name to Write: ";
            let chars_before_cursor: String = self.input_buffer.chars().take(self.save_input_cursor).collect();
            let cursor_x = prompt.chars().count() + chars_before_cursor.chars().count();
            target_x = cursor_x.min(width.saturating_sub(1)) as u16;
            target_y = (height - 2) as u16;
            cursor_visible = true;
        } else if self.mode == Mode::Open {
            let prompt = " File Name to Open: ";
            let prompt_text = format!("{}{}", prompt, self.input_buffer);
            target_x = prompt_text.chars().count().min(width.saturating_sub(1)) as u16;
            target_y = (self.terminal_size.1 - 3) as u16;
            cursor_visible = true;
        }

        if cursor_visible {
            queue!(
                stdout,
                cursor::MoveTo(target_x, target_y),
                cursor::Show
            )?;
        }

        stdout.flush()?;
        Ok(())
    }

    fn draw_top_bar(
        &self,
        stdout: &mut io::Stdout,
        width: usize,
    ) -> Result<()> {
        let filename = self
            .file_path
            .as_ref()
            .map(|p| {
                if p.is_absolute() {
                    p.display().to_string()
                } else {
                    env::current_dir()
                        .map(|dir| dir.join(p).display().to_string())
                        .unwrap_or_else(|_| p.display().to_string())
                }
            })
            .unwrap_or_else(|| "[No Name]".to_string());

        let mut inner = vec![' '; width];

        // Keep the application name/version on the left, the full path centered,
        // and the saved state on the right.
        let left = " rico 2026.9.27.0 ";
        for (i, ch) in left.chars().take(width).enumerate() {
            inner[i] = ch;
        }

        let state = if self.dirty { " UNSAVED " } else { " SAVED " };
        let right_start = width.saturating_sub(state.chars().count());
        for (i, ch) in state.chars().enumerate() {
            if right_start + i < width {
                inner[right_start + i] = ch;
            }
        }

        let centered_path = center_text(&filename, width);
        let left_end = left.chars().count();
        let right_start = width.saturating_sub(state.chars().count());
        for (i, ch) in centered_path.chars().enumerate() {
            if i >= left_end && i < right_start {
                inner[i] = ch;
            }
        }

        queue!(
            stdout,
            cursor::MoveTo(0, 0),
            SetBackgroundColor(CtColor::DarkGrey),
            SetForegroundColor(CtColor::White),
            SetAttribute(Attribute::Bold)
        )?;

        for ch in inner {
            queue!(stdout, Print(ch))?;
        }

        queue!(
            stdout,
            SetAttribute(Attribute::Reset),
            SetBackgroundColor(CtColor::Reset),
            SetForegroundColor(CtColor::Reset),
            Clear(ClearType::UntilNewLine)
        )?;

        Ok(())
    }

    fn draw_visual_line(
        &self,
        stdout: &mut io::Stdout,
        row_idx: usize,
        start_byte: usize,
        end_byte: usize,
        _width: usize,
        content_width: usize,
        gutter_width: usize,
        scroll_col: usize,
        selection: Option<(Position, Position)>,
        is_first_chunk: bool,
        decoded_query: &str,
    ) -> Result<()> {
        if self.show_line_numbers {
            if is_first_chunk {
                let line_num = row_idx + 1;
                let gutter = format!("{:>w$}  ", line_num, w = gutter_width.saturating_sub(2));
                queue!(
                    stdout,
                    SetForegroundColor(CtColor::DarkGrey),
                    Print(&gutter),
                    SetForegroundColor(CtColor::Reset)
                )?;
            } else {
                let gutter = " ".repeat(gutter_width);
                queue!(stdout, Print(&gutter))?;
            }
        }

        let line = &self.lines[row_idx];
        let mut visual_c = 0;

        let chunk_str = &line[start_byte..end_byte];
        let mut current_style = CellStyle::Normal;

        for (c_byte_idx, ch) in chunk_str.char_indices() {
            let absolute_byte_idx = start_byte + c_byte_idx;
            let char_width = if ch == '\t' { TAB_WIDTH } else { 1 };
            let next_visual = visual_c + char_width;

            if next_visual <= scroll_col {
                visual_c = next_visual;
                continue;
            }
            if visual_c >= scroll_col + content_width {
                break;
            }

            let highlighted = selection.map(|(s, e)| {
                if row_idx > s.row && row_idx < e.row { true }
                else if row_idx == s.row && row_idx == e.row { absolute_byte_idx >= s.col && absolute_byte_idx < e.col }
                else if row_idx == s.row { absolute_byte_idx >= s.col }
                else if row_idx == e.row { absolute_byte_idx < e.col }
                else { false }
            }).unwrap_or(false);

            let mut is_match = false;
            let mut is_active_match = false;

            if !decoded_query.is_empty() {
                for (match_idx, m) in self.find_matches.iter().enumerate() {
                    if m.row == row_idx {
                        let match_end_col = m.col + decoded_query.len();
                        if absolute_byte_idx >= m.col && absolute_byte_idx < match_end_col {
                            is_match = true;
                            if match_idx == self.find_index {
                                is_active_match = true;
                            }
                            break;
                        }
                    }
                }
            }

            let style = if highlighted {
                CellStyle::Selected
            } else if is_active_match {
                CellStyle::ActiveMatch
            } else if is_match {
                CellStyle::Match
            } else {
                CellStyle::Normal
            };

            if style != current_style {
                match current_style {
                    CellStyle::Selected | CellStyle::ActiveMatch => {
                        queue!(stdout, SetAttribute(Attribute::NoReverse))?;
                    }
                    CellStyle::Match => {
                        queue!(
                            stdout,
                            SetBackgroundColor(CtColor::Reset),
                            SetForegroundColor(CtColor::Reset)
                        )?;
                    }
                    CellStyle::Normal => {}
                }

                current_style = style;

                match current_style {
                    CellStyle::Selected => {
                        queue!(stdout, SetAttribute(Attribute::Reverse))?;
                    }
                    CellStyle::Match => {
                        queue!(
                            stdout,
                            SetBackgroundColor(CtColor::DarkYellow),
                            SetForegroundColor(CtColor::Black)
                        )?;
                    }
                    CellStyle::ActiveMatch => {
                        queue!(stdout, SetAttribute(Attribute::Reverse))?;
                    }
                    CellStyle::Normal => {}
                }
            }

            let overlap_start = visual_c.max(scroll_col) - scroll_col;
            let overlap_end = next_visual.min(scroll_col + content_width) - scroll_col;
            let overlap_width = overlap_end - overlap_start;

            if overlap_width > 0 {
                if ch == ' ' {
                    queue!(stdout, Print(" ".repeat(overlap_width)))?;
                } else if ch == '\t' {
                    let mut tab_str = String::new();
                    for i in visual_c..next_visual {
                        if i >= scroll_col && i < scroll_col + content_width {
                            if i == visual_c { tab_str.push('→'); }
                            else { tab_str.push(' '); }
                        }
                    }
                    queue!(stdout, SetForegroundColor(CtColor::Cyan), Print(tab_str), SetForegroundColor(CtColor::Reset))?;
                } else {
                    queue!(stdout, Print(ch))?;
                }
            }
            visual_c = next_visual;
        }

        if current_style != CellStyle::Normal {
            match current_style {
                CellStyle::Selected | CellStyle::ActiveMatch => {
                    queue!(stdout, SetAttribute(Attribute::NoReverse))?;
                }
                CellStyle::Match => {
                    queue!(
                        stdout,
                        SetBackgroundColor(CtColor::Reset),
                        SetForegroundColor(CtColor::Reset)
                    )?;
                }
                CellStyle::Normal => {}
            }
        }

        let is_last_chunk = end_byte == line.len();
        if is_last_chunk && row_idx < self.lines.len() - 1
            && visual_c >= scroll_col
            && visual_c < scroll_col + content_width
        {
            queue!(stdout, SetForegroundColor(CtColor::DarkYellow), Print('↵'), SetForegroundColor(CtColor::Reset))?;
        }

        queue!(stdout, Clear(ClearType::UntilNewLine))?;
        Ok(())
    }

    fn draw_status_bar(
        &self,
        stdout: &mut io::Stdout,
        width: usize,
    ) -> Result<()> {
        let text = match self.mode {
            Mode::Open => format!(" File Name to Open: {} ", self.input_buffer),
            _ => format!(" {} ", self.status_message),
        };
        let warning = self.status_message.contains("UNSAVED");

        let status_items = if self.mode == Mode::Normal {
            let tab = self.tab_style.description();
            let wrap_state = if self.word_wrap { "WRAP" } else { "NOWRAP" };
            let line_ending = match self.line_ending {
                LineEnding::Lf => "LF",
                LineEnding::Crlf => "CRLF",
            };
            let total_chars = self.lines.iter().map(String::len).sum::<usize>()
                + self.lines.len().saturating_sub(1);

            Some(format!(
                " Ln {} Col {} | {} | Lines {} | Chars {} | {} | {} ",
                self.cursor.row + 1,
                self.cursor.col + 1,
                wrap_state,
                self.lines.len(),
                total_chars,
                tab,
                line_ending
            ))
        } else {
            None
        };

        let status_line = match status_items {
            Some(items) => center_text(&items, width),
            None => pad_to(&text, width),
        };

        queue!(
            stdout,
            cursor::MoveTo(0, (self.terminal_size.1 - 3) as u16),
            SetBackgroundColor(if warning { CtColor::DarkRed } else { CtColor::DarkGrey }),
            SetForegroundColor(CtColor::White),
            SetAttribute(Attribute::Bold),
            Print(status_line),
            SetAttribute(Attribute::Reset),
            SetBackgroundColor(CtColor::Reset),
            SetForegroundColor(CtColor::Reset),
            Clear(ClearType::UntilNewLine)
        )?;

        Ok(())
    }

    fn draw_find_replace_bars(
        &self,
        stdout: &mut io::Stdout,
        width: usize,
        height: usize,
    ) -> Result<()> {
        let find_active = self.find_field == FindField::Find;
        let replace_active = self.find_field == FindField::Replace;

        let match_status = if self.find_matches.is_empty() {
            if self.find_query.is_empty() {
                "".to_string()
            } else {
                " [No matches] ".to_string()
            }
        } else {
            format!(" [{}/{}] ", self.find_index + 1, self.find_matches.len())
        };

        let find_left = format!(" Find: {}{}", self.find_query, match_status);
        let find_right = "[↑] [↓]";
        let find_line = pad_with_right(&find_left, find_right, width);

        queue!(
            stdout,
            cursor::MoveTo(0, (height - 3) as u16),
            SetBackgroundColor(if find_active { CtColor::DarkYellow } else { CtColor::DarkGrey }),
            SetForegroundColor(if find_active { CtColor::Black } else { CtColor::White }),
            SetAttribute(Attribute::Bold),
            Print(find_line),
            SetAttribute(Attribute::Reset),
            SetBackgroundColor(CtColor::Reset),
            SetForegroundColor(CtColor::Reset),
            Clear(ClearType::UntilNewLine)
        )?;

        let replace_line = pad_to(&format!(" Replace: {}", self.replace_query), width);
        queue!(
            stdout,
            cursor::MoveTo(0, (height - 2) as u16),
            SetBackgroundColor(if replace_active { CtColor::DarkYellow } else { CtColor::DarkGrey }),
            SetForegroundColor(if replace_active { CtColor::Black } else { CtColor::White }),
            SetAttribute(Attribute::Bold),
            Print(replace_line),
            SetAttribute(Attribute::Reset),
            SetBackgroundColor(CtColor::Reset),
            SetForegroundColor(CtColor::Reset),
            Clear(ClearType::UntilNewLine)
        )?;

        let menu_items: &[&str] = &[
            "^Q Quit Find",
            "^P Prev",
            "^N Next",
            "^R Replace",
            "^A Replace All",
        ];
        let menu_line = format_menu_grid(&[menu_items], width).into_iter().next().unwrap_or_default();

        queue!(
            stdout,
            cursor::MoveTo(0, (height - 1) as u16),
            Print(menu_line),
            Clear(ClearType::UntilNewLine)
        )?;

        Ok(())
    }

    fn draw_save_bar(
        &self,
        stdout: &mut io::Stdout,
        width: usize,
        height: usize,
    ) -> Result<()> {
        queue!(
            stdout,
            cursor::MoveTo(0, (height - 3) as u16),
            SetBackgroundColor(CtColor::Reset),
            Clear(ClearType::UntilNewLine)
        )?;

        let save_left = format!(" File Name to Write: {} ", self.input_buffer);
        let save_right = "[Enter: Save] [Esc: Cancel]";
        let save_line = pad_with_right(&save_left, save_right, width);

        queue!(
            stdout,
            cursor::MoveTo(0, (height - 2) as u16),
            SetBackgroundColor(CtColor::DarkYellow),
            SetForegroundColor(CtColor::Black),
            SetAttribute(Attribute::Bold),
            Print(save_line),
            SetAttribute(Attribute::Reset),
            SetBackgroundColor(CtColor::Reset),
            SetForegroundColor(CtColor::Reset),
            Clear(ClearType::UntilNewLine)
        )?;

        let menu_items: &[&str] = &[
            "^Q / Esc: Cancel",
            "Enter: Write File",
        ];
        let menu_line = format_menu_grid(&[menu_items], width).into_iter().next().unwrap_or_default();

        queue!(
            stdout,
            cursor::MoveTo(0, (height - 1) as u16),
            Print(menu_line),
            Clear(ClearType::UntilNewLine)
        )?;

        Ok(())
    }

    fn draw_quit_warning_bar(
        &self,
        stdout: &mut io::Stdout,
        width: usize,
        height: usize,
    ) -> Result<()> {
        queue!(
            stdout,
            cursor::MoveTo(0, (height - 3) as u16),
            SetBackgroundColor(CtColor::Reset),
            Clear(ClearType::UntilNewLine)
        )?;

        let warning_line = center_text("UNSAVED CHANGES!", width);

        queue!(
            stdout,
            cursor::MoveTo(0, (height - 2) as u16),
            SetBackgroundColor(CtColor::DarkRed),
            SetForegroundColor(CtColor::White),
            SetAttribute(Attribute::Bold),
            Print(warning_line),
            SetAttribute(Attribute::Reset),
            SetBackgroundColor(CtColor::Reset),
            SetForegroundColor(CtColor::Reset),
            Clear(ClearType::UntilNewLine)
        )?;

        let menu_items: &[&str] = &[
            "^Q: Discard & Quit",
            "Any other key: Resume Editing",
        ];
        let menu_line = format_menu_grid(&[menu_items], width).into_iter().next().unwrap_or_default();

        queue!(
            stdout,
            cursor::MoveTo(0, (height - 1) as u16),
            Print(menu_line),
            Clear(ClearType::UntilNewLine)
        )?;

        Ok(())
    }

    fn draw_message_bar(
        &self,
        stdout: &mut io::Stdout,
        width: usize,
    ) -> Result<()> {
        let (menu_1, menu_2) = match self.mode {
            Mode::Open => (
                center_text("[ OPEN FILE MODE ]", width),
                center_text("Enter: Open file | Esc: Cancel", width),
            ),
            _ => {
                let row1: &[&str] = &[
                    "^Q Quit",
                    "^Z Undo",
                    "^C Copy",
                    "^X Cut",
                    "^N Line Nums",
                    "^K Cut Line",
                ];
                let row2: &[&str] = &[
                    "^S Save",
                    "^Y Redo",
                    "^V Paste",
                    "^F Find",
                    "^W Wrap",
                    "^U Paste Line",
                ];
                let formatted = format_menu_grid(&[row1, row2], width);
                (formatted[0].clone(), formatted[1].clone())
            }
        };

        queue!(
            stdout,
            cursor::MoveTo(0, (self.terminal_size.1 - 2) as u16),
            Print(menu_1),
            Clear(ClearType::UntilNewLine),
            cursor::MoveTo(0, (self.terminal_size.1 - 1) as u16),
            Print(menu_2),
            Clear(ClearType::UntilNewLine)
        )?;

        Ok(())
    }
}

fn format_menu_grid(rows: &[&[&str]], width: usize) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let num_cols = rows[0].len();
    let mut col_widths = vec![0; num_cols];
    for row in rows {
        for (c, item) in row.iter().enumerate() {
            col_widths[c] = col_widths[c].max(item.chars().count());
        }
    }

    let gap = "    ";
    let mut formatted_rows = Vec::new();

    for row in rows {
        let mut row_str = String::new();
        for (c, item) in row.iter().enumerate() {
            if c > 0 {
                row_str.push_str(gap);
            }
            let padded = pad_to(item, col_widths[c]);
            row_str.push_str(&padded);
        }
        formatted_rows.push(center_text(&row_str, width));
    }

    formatted_rows
}

fn decode_escapes(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('\\') => result.push('\\'),
                Some('0') => result.push('\0'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn offset_to_position(content: &str, byte_offset: usize) -> Position {
    let mut row = 0;
    let mut line_start_byte = 0;
    for (i, b) in content.bytes().enumerate() {
        if i >= byte_offset {
            break;
        }
        if b == b'\n' {
            row += 1;
            line_start_byte = i + 1;
        }
    }
    let col = byte_offset.saturating_sub(line_start_byte);
    Position { row, col }
}

fn insert_char_at(s: &str, char_idx: usize, c: char) -> (String, usize) {
    let mut chars: Vec<char> = s.chars().collect();
    let idx = min(char_idx, chars.len());
    chars.insert(idx, c);
    (chars.iter().collect(), idx + 1)
}

fn remove_char_before(s: &str, char_idx: usize) -> (String, usize) {
    let mut chars: Vec<char> = s.chars().collect();
    if char_idx > 0 && char_idx <= chars.len() {
        chars.remove(char_idx - 1);
        (chars.iter().collect(), char_idx - 1)
    } else {
        (s.to_string(), char_idx)
    }
}

fn remove_char_at(s: &str, char_idx: usize) -> (String, usize) {
    let mut chars: Vec<char> = s.chars().collect();
    if char_idx < chars.len() {
        chars.remove(char_idx);
        (chars.iter().collect(), char_idx)
    } else {
        (s.to_string(), char_idx)
    }
}

fn wrap_line(line: &str, width: usize, word_wrap: bool) -> Vec<(usize, usize)> {
    if !word_wrap || width == 0 {
        return vec![(0, line.len())];
    }
    if line.is_empty() {
        return vec![(0, 0)];
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    let mut current_width = 0;
    let mut last_space = None;

    for (idx, ch) in line.char_indices() {
        let w = if ch == '\t' { TAB_WIDTH } else { 1 };

        if current_width + w > width && current_width > 0 {
            if let Some((space_byte_idx, space_char_len)) = last_space {
                if space_byte_idx >= start {
                    let break_point = space_byte_idx + space_char_len;
                    chunks.push((start, break_point));
                    start = break_point;
                    current_width = visual_width_between(line, start, idx) + w;
                    last_space = None;
                } else {
                    chunks.push((start, idx));
                    start = idx;
                    current_width = w;
                    last_space = None;
                }
            } else {
                chunks.push((start, idx));
                start = idx;
                current_width = w;
            }
        } else {
            current_width += w;
        }

        if ch == ' ' || ch == '-' || ch == '_' {
            last_space = Some((idx, ch.len_utf8()));
        }
    }

    if start < line.len() || chunks.is_empty() {
        chunks.push((start, line.len()));
    }

    chunks
}

fn visual_width_between(text: &str, start_byte: usize, end_byte: usize) -> usize {
    let mut w = 0;
    for (idx, ch) in text.char_indices() {
        if idx >= end_byte { break; }
        if idx >= start_byte {
            w += if ch == '\t' { TAB_WIDTH } else { 1 };
        }
    }
    w
}

fn detect_line_ending(content: &str) -> LineEnding {
    if content.contains("\r\n") {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

fn parse_lines(content: &str) -> Vec<String> {
    if content.is_empty() {
        return vec![String::new()];
    }
    content
        .split('\n')
        .map(|s| s.trim_end_matches('\r').to_string())
        .collect()
}

fn detect_tab_style(lines: &[String]) -> TabStyle {
    let has_leading_tabs = lines.iter().any(|line| line.starts_with('\t'));
    if has_leading_tabs {
        return TabStyle::Tabs;
    }

    let mut indentation_widths = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let spaces = line.chars().take_while(|c| *c == ' ').count();
        if spaces > 0 {
            indentation_widths.push(spaces);
        }
    }

    if indentation_widths.is_empty() {
        return TabStyle::Tabs;
    }

    let mut width = indentation_widths[0];
    for value in indentation_widths.iter().skip(1) {
        width = gcd(width, *value);
        if width == 1 {
            break;
        }
    }

    if !matches!(width, 1 | 2 | 3 | 4 | 5 | 6 | 8) {
        width = indentation_widths.iter().copied().min().unwrap_or(4).min(8);
    }
    if width == 1 {
        width = 2;
    }
    TabStyle::Spaces(width)
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    a
}

fn visual_width_until(text: &str, byte_col: usize) -> usize {
    let safe_col = min(byte_col, text.len());
    let mut width = 0;

    for (idx, ch) in text.char_indices() {
        if idx >= safe_col {
            break;
        }
        width += if ch == '\t' { TAB_WIDTH } else { 1 };
    }
    width
}

fn center_text(text: &str, width: usize) -> String {
    let text_len = text.chars().count();
    if text_len >= width {
        return text.chars().take(width).collect();
    }
    let left = (width - text_len) / 2;
    let right = width - text_len - left;
    format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
}

fn pad_to(text: &str, width: usize) -> String {
    let mut result = text.chars().take(width).collect::<String>();
    if result.chars().count() < width {
        result.push_str(&" ".repeat(width - result.chars().count()));
    }
    result
}

fn pad_with_right(left: &str, right: &str, width: usize) -> String {
    let left_chars: Vec<char> = left.chars().collect();
    let right_chars: Vec<char> = right.chars().collect();
    let left_len = left_chars.len();
    let right_len = right_chars.len();

    if left_len + right_len >= width {
        let mut s: String = left_chars.into_iter().take(width).collect();
        if s.chars().count() > width {
            s = s.chars().take(width).collect();
        }
        return s;
    }

    let spaces = width - left_len - right_len;
    let mut result: String = left_chars.into_iter().collect();
    result.push_str(&" ".repeat(spaces));
    result.push_str(&right_chars.into_iter().collect::<String>());
    result
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    let (path, content) = if args.len() > 1 {
        let path = PathBuf::from(&args[1]);
        let path = if path.is_absolute() {
            path
        } else {
            fs::canonicalize(&path).unwrap_or_else(|_| {
                env::current_dir()
                    .map(|dir| dir.join(&path))
                    .unwrap_or(path)
            })
        };
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::File::create(&path).with_context(|| {
                    format!("failed to create {}", path.display())
                })?;
                String::new()
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read {}", path.display())
                });
            }
        };
        (Some(path), content)
    } else {
        (None, String::new())
    };

    let mut editor = Editor::new(path, content);
    editor.run().context("rico terminated unexpectedly")
}