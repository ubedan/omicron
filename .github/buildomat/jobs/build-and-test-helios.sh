#!/bin/bash
#:
#: name = "build-and-test (helios)"
#: variety = "basic"
#: target = "helios-2.0-20231118"
#: rust_toolchain = "1.72.1"
#: output_rules = [
#:	"%/work/*",
#:	"%/var/tmp/omicron_tmp/*",
#:	"!/var/tmp/omicron_tmp/crdb-base*",
#:	"!/var/tmp/omicron_tmp/rustc*",
#: ]

exec .github/buildomat/build-and-test.sh illumos
