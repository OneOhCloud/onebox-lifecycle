BINARY := demo_full

.PHONY: all debug release clean run

all: debug

debug:
	cargo build --example $(BINARY)
	cp target/debug/examples/$(BINARY) .

release:
	cargo build --release --example $(BINARY)
	cp target/release/examples/$(BINARY) .

run: debug
	./$(BINARY)

clean:
	cargo clean
	rm -f $(BINARY)
