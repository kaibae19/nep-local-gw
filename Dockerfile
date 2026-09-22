# Stage 1: Chef - Setup cargo-chef base
FROM rust:alpine AS chef
# Install build dependencies
RUN apk add --no-cache musl-dev pkgconfig cmake make g++ git openssl-dev perl
RUN cargo install --locked cargo-chef
WORKDIR /app

# Stage 2: Planner - Prepare cargo-chef recipe
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 3: Builder - Cook dependencies & compile application
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Build the actual application
COPY . .
RUN cargo build --release --bin nep-gw

# Stage 4: Runtime - Minimal alpine image
FROM alpine:latest AS runtime
RUN apk add --no-cache ca-certificates tzdata
WORKDIR /app
COPY --from=builder /app/target/release/nep-gw /usr/local/bin/nep-gw

EXPOSE 8080

CMD ["/usr/local/bin/nep-gw"]
