default: run

build:
	podman build -t rico .
	cargo build --release

run: build
	touch target/release/test.txt target/release/test2.txt
	podman run -it --rm -v ./target/release:/code:z -w /code rico ./rico test.txt test2.txt

clean:
	podman rmi rico
