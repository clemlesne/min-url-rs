# syntax=docker/dockerfile:1
# check=skip=UndefinedVar

# Rust version
ARG RUST_VERSION=1.86.0

# Build container
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

# Setup user context
ENV HOME=/app-root
WORKDIR ${HOME}

# Copy sources
COPY . .

# Source platform from buildx "platform" argument
ARG TARGETPLATFORM

# Service to package
ARG SERVICE_NAME

# Build
RUN --mount=target=${HOME}/target,type=cache,id=target-${TARGETPLATFORM},sharing=locked --mount=target=${HOME}/.cargo/git,type=cache,id=cargo-git-${TARGETPLATFORM},sharing=locked --mount=target=${HOME}/.cargo/registry,type=cache,id=cargo-registry-${TARGETPLATFORM},sharing=locked \
    cargo build --release --package ${SERVICE_NAME} \
    && mv target/release/${SERVICE_NAME} service

# Output container
FROM debian:bookworm-slim AS final

# Setup user
RUN adduser \
    --disabled-password \
    --gecos "" \
    --home "/nonexistent" \
    --no-create-home \
    --shell "/sbin/nologin" \
    --uid "10001" \
    appuser

# Copy binary
COPY --from=builder --chown=appuser:appuser /app-root/service /usr/local/bin

# Setup environment
ENTRYPOINT ["service"]
EXPOSE 8080/tcp
USER appuser

# Configure logging
ENV RUST_LOG="${SERVICE_NAME}=debug,info"
