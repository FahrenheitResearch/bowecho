# syntax=docker/dockerfile:1.7

# Platform-specific manifest digests are used so an amd64 numerical stack is
# selected even when the builder itself is multi-architecture.
FROM rust:1.85.1-slim-bookworm@sha256:3490aa77d179a59d67e94239cca96dd84030b564470859200f535b942bdffedf AS rust_toolchain
FROM python:3.11.9-slim-bookworm@sha256:2856e6af199e8128161abd320575eb9b341f3b76f017b5d0c9cd364f60d8a050

ARG DEBIAN_SNAPSHOT=20240904T000000Z

ENV LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    TZ=UTC \
    SOURCE_DATE_EPOCH=1725408000 \
    PYTHONHASHSEED=0 \
    PYTHONDONTWRITEBYTECODE=1 \
    OPENBLAS_NUM_THREADS=1 \
    OMP_NUM_THREADS=1 \
    MKL_NUM_THREADS=1 \
    NUMEXPR_NUM_THREADS=1 \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:${PATH}

COPY --from=rust_toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust_toolchain /usr/local/rustup /usr/local/rustup

# Debian snapshot URLs are immutable. check-valid-until is disabled because
# archived Release metadata is deliberately older than the build date.
RUN rm -f /etc/apt/sources.list.d/debian.sources \
    && printf '%s\n' \
       "deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/${DEBIAN_SNAPSHOT}/ bookworm main" \
       > /etc/apt/sources.list \
    && apt-get -o Acquire::Check-Valid-Until=false update \
    && apt-get install -y --no-install-recommends \
       build-essential=12.9 \
       gfortran=4:12.2.0-3 \
       gfortran-12=12.2.0-14 \
       libgfortran5=12.2.0-14 \
    && rm -rf /var/lib/apt/lists/*

COPY crates/radar_scattering/tools/pytmatrix-0.3.3/requirements-bootstrap-pinned.txt /opt/pytmatrix/requirements-bootstrap-pinned.txt
COPY crates/radar_scattering/tools/pytmatrix-0.3.3/requirements-pytmatrix-pinned.txt /opt/pytmatrix/requirements-pytmatrix-pinned.txt

ARG PYTMATRIX_COMMIT=22432e7468f6fc0848be54b13016a846a4518979
ARG PYTMATRIX_ARCHIVE_SHA256=65345df72b6bad5585ded6bb62edf87b6dfa7d7cb35d92dbf16e4b82739fca97

RUN python -m pip install --require-hashes -r /opt/pytmatrix/requirements-bootstrap-pinned.txt \
    && python -c "import urllib.request; urllib.request.urlretrieve('https://codeload.github.com/jleinonen/pytmatrix/tar.gz/${PYTMATRIX_COMMIT}', '/opt/pytmatrix/source.tar.gz')" \
    && echo "${PYTMATRIX_ARCHIVE_SHA256}  /opt/pytmatrix/source.tar.gz" | sha256sum -c - \
    && mkdir -p /opt/pytmatrix/source \
    && tar -xzf /opt/pytmatrix/source.tar.gz --strip-components=1 -C /opt/pytmatrix/source \
    && python -m pip install --no-deps --no-build-isolation /opt/pytmatrix/source \
    && python -m unittest pytmatrix.test.test_tmatrix

# The emitter is compiled from the same radar_scattering schema implementation
# that consumes the file. Only this small package and its transitive deps build.
WORKDIR /opt/source
COPY Cargo.toml /opt/source/Cargo.toml
COPY crates/radar_scattering/Cargo.toml /opt/source/crates/radar_scattering/Cargo.toml
COPY crates/radar_scattering/src /opt/source/crates/radar_scattering/src
COPY crates/radar_scattering/tools/pytmatrix-0.3.3/emitter /opt/source/crates/radar_scattering/tools/pytmatrix-0.3.3/emitter
RUN cargo build --locked --release --bins \
      --manifest-path /opt/source/crates/radar_scattering/tools/pytmatrix-0.3.3/emitter/Cargo.toml \
    && cp /opt/source/crates/radar_scattering/tools/pytmatrix-0.3.3/emitter/target/release/brslut-emitter /usr/local/bin/brslut-emitter \
    && cp /opt/source/crates/radar_scattering/tools/pytmatrix-0.3.3/emitter/target/release/validate_tmatrix_lut /usr/local/bin/validate_tmatrix_lut \
    && rm -rf /opt/source/crates/radar_scattering/tools/pytmatrix-0.3.3/emitter/target /usr/local/cargo/registry /usr/local/cargo/git

WORKDIR /workspace
CMD ["python", "crates/radar_scattering/tools/pytmatrix-0.3.3/generate_lut.py", "--help"]
