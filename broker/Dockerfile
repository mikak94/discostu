# Build from the repository root (the broker shares the workspace):
#   docker build -f broker/Dockerfile .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p discostu-broker

FROM debian:bookworm-slim
COPY --from=build /src/target/release/discostu-broker /usr/local/bin/discostu-broker
ENV BROKER_DATA=/data
EXPOSE 47900/udp
CMD ["discostu-broker"]
