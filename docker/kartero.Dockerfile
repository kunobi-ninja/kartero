FROM rust:1.95-bookworm AS builder

WORKDIR /app
COPY . .
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot

ARG BUILD_VERSION=dev
ARG BUILD_COMMIT=unknown
ARG BUILD_DATE=unknown

LABEL org.opencontainers.image.title="kartero"
LABEL org.opencontainers.image.description="Pulls OTLP JSON from CI artifacts and delivers it to an OTLP/HTTP backend"
LABEL org.opencontainers.image.version="${BUILD_VERSION}"
LABEL org.opencontainers.image.revision="${BUILD_COMMIT}"
LABEL org.opencontainers.image.created="${BUILD_DATE}"
LABEL org.opencontainers.image.source="https://github.com/kunobi-ninja/kartero"

COPY --from=builder /app/target/release/kartero /kartero

EXPOSE 8080

ENTRYPOINT ["/kartero"]
