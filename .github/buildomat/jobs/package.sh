#!/bin/bash
#:
#: name = "helios / package"
#: variety = "basic"
#: target = "helios-2.0"
#: rust_toolchain = "1.72.0"
#: output_rules = [
#:	"=/work/package.tar.gz",
#:	"=/work/zones/*.tar.gz",
#: ]

set -o errexit
set -o pipefail
set -o xtrace

cargo --version
rustc --version

ptime -m ./tools/install_builder_prerequisites.sh -yp
ptime -m ./tools/ci_download_softnpu_machinery

# Build the test target
ptime -m cargo run --locked --release --bin omicron-package -- \
  -t test target create -i standard -m non-gimlet -s softnpu
ptime -m cargo run --locked --release --bin omicron-package -- \
  -t test package

# Assemble some utilities into a tarball that can be used by deployment
# phases of buildomat.

files=(
	out/target/test
	out/npuzone/*
	package-manifest.toml
	smf/sled-agent/non-gimlet/config.toml
	target/release/omicron-package
	tools/create_virtual_hardware.sh
    tools/virtual_hardware.sh
	tools/scrimlet/*
)

pfexec mkdir -p /work && pfexec chown $USER /work
ptime -m tar cvzf /work/package.tar.gz "${files[@]}"

# Build necessary for the global zone
ptime -m cargo run --locked --release --bin omicron-package -- \
  -t host target create -i standard -m gimlet -s asic
ptime -m cargo run --locked --release --bin omicron-package -- \
  -t host package

# Non-Global Zones

# Assemble Zone Images into their respective output locations.
#
# Zones that are included into another are intentionally omitted from this list
# (e.g., the switch zone tarballs contain several other zone tarballs: dendrite,
# mg-ddm, etc.).
#
# Note that when building for a real gimlet, `propolis-server` and `switch-*`
# should be included in the OS ramdisk.
mkdir -p /work/zones
zones=(
  out/clickhouse.tar.gz
  out/clickhouse_keeper.tar.gz
  out/cockroachdb.tar.gz
  out/crucible-pantry.tar.gz
  out/crucible.tar.gz
  out/external-dns.tar.gz
  out/internal-dns.tar.gz
  out/omicron-nexus.tar.gz
  out/oximeter-collector.tar.gz
  out/propolis-server.tar.gz
  out/switch-*.tar.gz
  out/ntp.tar.gz
  out/omicron-gateway-softnpu.tar.gz
  out/omicron-gateway-asic.tar.gz
  out/overlay.tar.gz
)
cp "${zones[@]}" /work/zones/
