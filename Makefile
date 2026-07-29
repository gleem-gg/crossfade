APP_ID  := gg.gleem.Crossfade
BIN     := gleem-crossfade
PREFIX  ?= $(HOME)/.local
BINDIR  := $(DESTDIR)$(PREFIX)/bin
APPDIR  := $(DESTDIR)$(PREFIX)/share/applications
ICONDIR := $(DESTDIR)$(PREFIX)/share/icons/hicolor/scalable/apps

.PHONY: all build run check clean install uninstall

all: build

build:
	cargo build --release

run:
	cargo run

check:
	cargo clippy -- -D warnings
	desktop-file-validate data/$(APP_ID).desktop

clean:
	cargo clean

install: build
	install -Dm755 target/release/$(BIN) $(BINDIR)/$(BIN)
	install -Dm644 data/$(APP_ID).desktop $(APPDIR)/$(APP_ID).desktop
	install -Dm644 data/icons/hicolor/scalable/apps/$(APP_ID).svg \
		$(ICONDIR)/$(APP_ID).svg
	-update-desktop-database -q $(APPDIR) 2>/dev/null
	-gtk4-update-icon-cache -f -t $(DESTDIR)$(PREFIX)/share/icons/hicolor 2>/dev/null

uninstall:
	rm -f $(BINDIR)/$(BIN)
	rm -f $(APPDIR)/$(APP_ID).desktop
	rm -f $(ICONDIR)/$(APP_ID).svg
	-update-desktop-database -q $(APPDIR) 2>/dev/null
