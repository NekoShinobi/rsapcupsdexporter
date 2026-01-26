FROM ghcr.io/nekoshinobi/rust-chef-sccache:main AS base
FROM base AS planner
WORKDIR /app
COPY . .
RUN --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo chef prepare --recipe-path recipe.json

FROM base AS builder
WORKDIR /app

RUN apt update && apt install -y musl-tools
RUN rustup target add x86_64-unknown-linux-musl
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo chef cook --release --target x86_64-unknown-linux-musl --recipe-path recipe.json

COPY . .
RUN --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
WORKDIR /app

COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/rsapcupsdexporter rsapcupsdexporter

CMD ["/app/rsapcupsdexporter"]
