FROM alpine:latest

ARG BINARY_FILE=./target/x86_64-unknown-linux-musl/release/llr_sync
COPY ${BINARY_FILE} ./llr_sync

ENV RUST_LOG=tower_http=trace

ENTRYPOINT ["./llr_sync"]
