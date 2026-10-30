# Build and install meersh. `make install PREFIX=~/.local` installs for one user.
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
CARGO ?= cargo

.PHONY: build test install uninstall

build:
	$(CARGO) build --release --workspace

test:
	$(CARGO) test --workspace

install: build
	install -Dm755 target/release/meersh $(DESTDIR)$(BINDIR)/meersh
	install -Dm755 target/release/meer-box $(DESTDIR)$(BINDIR)/meer-box
	install -Dm644 config.example.toml $(DESTDIR)$(PREFIX)/share/doc/meersh/config.example.toml

uninstall:
	rm -f $(DESTDIR)$(BINDIR)/meersh $(DESTDIR)$(BINDIR)/meer-box
