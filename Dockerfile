FROM rust:slim-bullseye as builder

WORKDIR /app
COPY . .
RUN apt update
RUN apt install gcc git make -y
RUN cargo clean
RUN cargo build --release --features jemalloc

FROM debian:bullseye-slim as runtime

RUN apt update
COPY --from=builder /app/target/release/packetcrypt /usr/local/bin/packetcrypt

ENTRYPOINT [ "packetcrypt" ]
