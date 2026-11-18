# Runs `cargo test -p dbstudio-driver-oracle` against the oraclefree service (see compose.test.yml).
#
# Oracle Instant Client: the base image installs it in /usr/lib/oracle/23/client64/lib and registers
# that directory in /etc/ld.so.conf.d (ldconfig), so ODPI-C (the `oracle` crate) finds libclntsh at
# runtime without LD_LIBRARY_PATH. ORACLE_CLIENT_LIB_DIR is exported with the same directory for
# tests that pass it to `InitParams::oracle_client_lib_dir`.
#
# The base image is linux/amd64 only (emulated on Apple Silicon: the first build/compile is slow).
# Cargo output goes to the named volume mounted at /target-oracle, the repo is mounted at /work.
FROM ghcr.io/oracle/oraclelinux9-instantclient:23

# yum.oracle.com is sometimes unreachable from CI runners for a short while (curl error 7 on the
# repo metadata): retry with growing pauses instead of failing the whole integration job.
RUN for attempt in 1 2 3 4 5; do \
      dnf install -y --setopt=retries=10 --setopt=timeout=60 \
        gcc gcc-c++ make pkgconf-pkg-config openssl-devel curl git ca-certificates \
        && break; \
      if [ "$attempt" = 5 ]; then exit 1; fi; \
      echo "dnf install failed (attempt $attempt), retrying in $((attempt * 20))s" >&2; \
      dnf clean all; sleep $((attempt * 20)); \
    done \
    && dnf clean all

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/root/.cargo \
    PATH=/root/.cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable

ENV CARGO_TARGET_DIR=/target-oracle \
    ORACLE_CLIENT_LIB_DIR=/usr/lib/oracle/23/client64/lib

WORKDIR /work
CMD ["cargo", "test", "-p", "dbstudio-driver-oracle"]
