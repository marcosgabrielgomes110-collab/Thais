FROM rust:1.99-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked --workspace

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates git && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/thais /usr/local/bin/thais
COPY --from=build /src/target/release/thais-sandbox /usr/local/bin/thais-sandbox
ENTRYPOINT ["thais"]
