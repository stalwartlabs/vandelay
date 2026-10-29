FROM rust:1.99-alpine AS builder
RUN apk add --no-cache musl-dev ca-certificates
WORKDIR /usr/src/vandelay
COPY . .
RUN cargo build --release --locked \
    && echo "nobody:x:65534:65534:nobody:/:/sbin/nologin" > /etc/passwd.minimal

FROM scratch
COPY --from=builder /usr/src/vandelay/target/release/vandelay /vandelay
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /etc/passwd.minimal /etc/passwd
USER nobody
WORKDIR /data
ENTRYPOINT ["/vandelay"]
