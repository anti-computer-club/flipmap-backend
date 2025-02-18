# build binary
FROM rust:latest as builder
WORKDIR /app
COPY . .
RUN cargo build --release

# copy + run binary
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libssl3
WORKDIR /app
COPY --from=builder /app/target/release/hello_osm .
CMD ["./hello_osm", "0.0.0.0", 80]

#
# 	linux-vdso.so.1 (0x0000779eb8bb6000)
# 	libssl.so.3 => /usr/lib/libssl.so.3 (0x0000779eb8324000)
# 	libcrypto.so.3 => /usr/lib/libcrypto.so.3 (0x0000779eb7e00000)
# 	libgcc_s.so.1 => /usr/lib/libgcc_s.so.1 (0x0000779eb8b44000)
# 	libm.so.6 => /usr/lib/libm.so.6 (0x0000779eb7d08000)
# 	libc.so.6 => /usr/lib/libc.so.6 (0x0000779eb7b16000)
# 	/lib64/ld-linux-x86-64.so.2 => /usr/lib64/ld-linux-x86-64.so.2 (0x0000779eb8bb8000)
# 
