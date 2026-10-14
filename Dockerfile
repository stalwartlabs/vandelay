# Step 1: Build the binary using the official Rust image
FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev git ca-certificates
RUN rustup target add x86_64-unknown-linux-musl

WORKDIR /usr/src/vandelay
COPY . .

# Create a minimal passwd file containing ONLY the 'nobody' user
RUN echo "nobody:x:65534:65534:nobody:/:/sbin/nologin" > /etc/passwd.minimal

# Force a fully static, release compilation targeting musl
RUN RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --target x86_64-unknown-linux-musl

RUN chmod +x /usr/src/vandelay/target/x86_64-unknown-linux-musl/release/vandelay
# Step 2: Copy the binary into a tiny, secure, stateless image
FROM scratch
COPY --from=builder /usr/src/vandelay/target/x86_64-unknown-linux-musl/release/vandelay /vandelay
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /etc/passwd.minimal /etc/passwd
USER nobody
ENTRYPOINT ["/vandelay"]
