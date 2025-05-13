install:
	@echo "➡️ Setting up redirect-svc"
	cargo install --locked --path redirect-svc
	
	@echo "➡️ Setting up slug-filler"
	cargo install --locked --path slug-filler

	@echo "➡️ Setting up write-svc"
	cargo install --locked --path write-svc

upgrade:
	@echo "➡️ Upgrading redirect-svc"
	cargo install --force --path redirect-svc

	@echo "➡️ Upgrading slug-filler"
	cargo install --force --path slug-filler

	@echo "➡️ Upgrading write-svc"
	cargo install --force --path write-svc

dev:
	@echo "➡️ Running dev services"
	docker-compose -f docker-compose.yaml up --build

clean:
	@echo "➡️ Cleaning up dev services"
	docker compose down -v
