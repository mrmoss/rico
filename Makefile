default: run

build:
	podman build -t rico .
	cargo build --release

touch-files:
	touch target/release/test.txt target/release/test2.txt

run: build touch-files
	podman run -it --rm -v ./target/release:/code:z -w /code rico ./rico -d test.txt test2.txt

bash: build touch-files
	podman run -it --rm -v ./target/release:/code:z -w /code rico /bin/bash

clean:
	podman rmi rico
