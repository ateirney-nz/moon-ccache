.PHONY: help build release test fmt fmt-check lint check clean install

# Default target
.DEFAULT_GOAL := help

# Configuration
CARGO := cargo
RUSTFLAGS ?=
TARGET ?=
INSTALL_PATH ?= ~/.local/bin

help: ## Show this help message
	@echo "ccache — Moon build command cache"
	@echo ""
	@echo "Available targets:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-20s %s\n", $$1, $$2}'

build: ## Build release binary (pass TARGET=<triple> for cross-compilation)
	@echo "Building ccache..."
	@if [ -n "$(TARGET)" ]; then \
		$(CARGO) build --release --target $(TARGET); \
	else \
		$(CARGO) build --release; \
	fi

release: build ## Alias for build target

test: ## Run all tests
	@echo "Running tests..."
	$(CARGO) test --all --verbose

test-doc: ## Run documentation tests
	@echo "Running doc tests..."
	$(CARGO) test --doc

fmt: ## Format code with rustfmt
	@echo "Formatting code..."
	$(CARGO) fmt --all

fmt-check: ## Check code formatting without making changes
	@echo "Checking code formatting..."
	$(CARGO) fmt --all -- --check

lint: ## Run clippy linter
	@echo "Running clippy..."
	$(CARGO) clippy --all --all-targets --all-features -- -W clippy::all -D warnings

check: fmt-check lint test ## Run all checks (format, lint, test)
	@echo "✓ All checks passed"

clean: ## Remove build artifacts
	@echo "Cleaning build artifacts..."
	$(CARGO) clean
	@rm -rf target/

install: build ## Install binary to ~/.local/bin
	@echo "Installing ccache to $(INSTALL_PATH)..."
	@mkdir -p $(INSTALL_PATH)
	@cp target/release/ccache $(INSTALL_PATH)/ccache
	@chmod +x $(INSTALL_PATH)/ccache
	@echo "✓ Installed to $(INSTALL_PATH)/ccache"
	@echo "  Make sure $(INSTALL_PATH) is in your PATH"

doc: ## Generate and open documentation
	@echo "Generating documentation..."
	$(CARGO) doc --all --no-deps --open

update: ## Update dependencies
	@echo "Updating dependencies..."
	$(CARGO) update

.PHONY: help build release test test-doc fmt fmt-check lint check clean install doc update
