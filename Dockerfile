FROM rust:1.98.0-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends cmake clang libclang-dev git && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN git clone --filter=blob:none https://github.com/chris124567/cfsolve vendor/cfsolve \
 && git -C vendor/cfsolve checkout ee4031fc9eb65702718f7ffbfedb451ad6bba541 \
 && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=build /src/target/release/byeflare /app/byeflare
COPY --from=build /src/vendor/cfsolve/profiles/safari-26-ios.json /app/profiles/safari-26-ios.json
COPY --from=build /src/vendor/cfsolve/profiles/safari-26-ios /app/profiles/safari-26-ios
ENV BYEFLARE_PROFILE=/app/profiles/safari-26-ios.json
CMD ["/app/byeflare"]
