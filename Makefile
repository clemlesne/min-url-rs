install:
	@echo "➡️ Setting up Clippy"
	rustup component add clippy

	@echo "➡️ Setting up dependencies"
	cargo install --locked --path redirect-svc
	cargo install --locked --path slug-filler
	cargo install --locked --path write-svc

upgrade:
	@echo "➡️ Upgrading dependencies"
	cargo update

dev:
	@echo "➡️ Running dev services"
	docker-compose -f docker-compose.yaml up --build

clean:
	@echo "➡️ Cleaning up dev services"
	docker compose down -v

lint:
	@echo "➡️ Running clippy"
	cargo clippy --frozen --fix

test:
	@echo "➡️ Running Clippy"
	cargo clippy --frozen
