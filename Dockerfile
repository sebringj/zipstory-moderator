FROM rust:1.84.1-bookworm as builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ffmpeg libssl3
COPY --from=builder /app/target/release/moderator /usr/local/bin/moderator
EXPOSE 8080
CMD ["moderator"]