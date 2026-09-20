#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
test_dir=$(mktemp -d /tmp/easytier-tunnel-tests.XXXXXX)
trap 'rm -rf "$test_dir"' EXIT
xcrun swiftc -emit-module -emit-library -module-name EasyTierShared \
  -module-cache-path "$test_dir/cache" EasyTierShared/EasyTierShared.swift \
  -o "$test_dir/libEasyTierShared.dylib"
xcrun clang -I EasyTierNetworkExtension -c tests/tunnel_ffi_stubs.c -o "$test_dir/stubs.o"
# Keep private lifecycle methods private in production. Appending the tests to
# the same Swift file lets the harness drive the real provider without a VPN.
cat EasyTierNetworkExtension/PacketTunnelProvider.swift tests/TunnelLifecycleTests.swift > "$test_dir/ProviderTests.swift"
xcrun swiftc -swift-version 5 -parse-as-library \
  -I "$test_dir" -L "$test_dir" -lEasyTierShared -Xlinker -rpath -Xlinker "$test_dir" \
  -module-cache-path "$test_dir/cache" -I EasyTierNetworkExtension \
  -import-objc-header tests/tunnel_ffi_stubs.h \
  "$test_dir/ProviderTests.swift" tests/PacketTunnelProviderStub.swift \
  EasyTierNetworkExtension/{AddressHelper,BuilderHelper,InfoModels,OSLogExporter,TunnelHelper}.swift \
  "$test_dir/stubs.o" -o "$test_dir/web-tests"
"$test_dir/web-tests"
