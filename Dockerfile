FROM rust:1.99 AS builder
RUN apt-get update \
    && apt-get install --yes --no-install-recommends musl-tools clang libclang-dev ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /usr/src/vandelay
COPY . .
RUN target="$(rustc -vV | sed -n 's/^host: //p' | sed 's/-gnu$/-musl/')" \
    && rustup target add "$target" \
    && CC=musl-gcc cargo build --release --locked --target "$target" \
    && cp "target/$target/release/vandelay" /vandelay \
    && echo "nobody:x:65534:65534:nobody:/:/sbin/nologin" > /etc/passwd.minimal

FROM scratch
COPY --from=builder /vandelay /vandelay
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder /etc/passwd.minimal /etc/passwd
USER nobody
WORKDIR /data
ENTRYPOINT ["/vandelay"]
