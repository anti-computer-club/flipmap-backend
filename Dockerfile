# build binary
FROM rust:1.87.0-bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

# copy + run binary
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libssl3 ca-certificates
WORKDIR /app
COPY --from=builder /app/target/release/flipmap-backend .
CMD ["./flipmap-backend"]
