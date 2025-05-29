# build binary
FROM rust:bookworm AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

# copy + run binary
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libssl3 ca-certificates
WORKDIR /app
COPY --from=builder /app/target/release/flipmap-backend .
CMD ["./flipmap-backend"]
