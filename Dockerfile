# syntax=docker/dockerfile:1.7

ARG RUST_IMAGE=rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1
ARG DEBIAN_IMAGE=debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

FROM ${RUST_IMAGE} AS builder

WORKDIR /build

COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY rust-toolchain.toml rust-toolchain.toml
COPY crates/ crates/
COPY vendor/ vendor/
COPY ci/ ci/

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/cargo-target,sharing=locked \
    set -eu; \
    expected_channel="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' rust-toolchain.toml)"; \
    test -n "$expected_channel"; \
    case "$(rustc -V)" in \
        "rustc $expected_channel "*) ;; \
        *) echo "error: rustc does not match rust-toolchain.toml channel $expected_channel" >&2; exit 1 ;; \
    esac; \
    CARGO_TARGET_DIR=/cargo-target \
    CARGO_PROFILE_RELEASE_DEBUG=0 \
    CARGO_PROFILE_RELEASE_STRIP=symbols \
    cargo build --release --locked -p htapd; \
    ci/check-htapd-no-crashsim.sh; \
    mkdir -p /out; \
    cp /cargo-target/release/htapd /out/htapd

FROM ${DEBIAN_IMAGE}

ARG VCS_REF=unknown

LABEL org.opencontainers.image.source="https://github.com/scarecrowgit/fluidb" \
      org.opencontainers.image.revision="${VCS_REF}" \
      org.opencontainers.image.licenses="Apache-2.0"

RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends tini=0.19.0-1+b3; \
    rm -rf /var/lib/apt/lists/*; \
    command -v bash; \
    command -v head; \
    command -v od; \
    command -v timeout; \
    groupadd --gid 10001 htapd; \
    useradd --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin htapd; \
    mkdir -p /var/lib/htapd; \
    chown htapd:htapd /var/lib/htapd

COPY --chmod=0555 --from=builder /out/htapd /usr/local/bin/htapd
COPY --chmod=0555 docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY --chmod=0555 docker/healthcheck.sh /usr/local/bin/healthcheck.sh

USER 10001

EXPOSE 3307

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/entrypoint.sh"]
CMD ["htapd", "--root", "/var/lib/htapd", "--listen", "0.0.0.0:3307"]

HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=5 CMD ["/usr/local/bin/healthcheck.sh"]
