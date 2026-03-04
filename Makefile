CARGO := cargo

all: build

build:
	cargo build --release

sanity: clippy test

test:
	@$(CARGO) test -q

clippy:
	@$(CARGO) clippy -q --color=always -- -Dwarnings

coverage:
	@$(CARGO) tarpaulin --out=html --output-dir /tmp/cov-output && \
		type -p open && cmd=open || type -p xdg-open && cmd=xdg-open; \
		$$cmd /tmp/cov-output/tarpaulin-report.html

website-install:
	cd website && npm install

website-dev:
	cd website && npm run dev

website-build:
	cd website && npm run build

website-preview: website-build
	cd website && npm run preview

.PHONY: all build sanity test clippy coverage website-install website-dev website-build website-preview
