// Lets MapLibre (and fetch) talk plain HTTP to the in-app server, and to
// nothing else.
//
// Android refuses cleartext by default; this allows it for 127.0.0.1 only,
// through a network security config, rather than usesCleartextTraffic="true"
// for every host. iOS allows loopback through NSAllowsLocalNetworking.
const { withAndroidManifest, withDangerousMod, withInfoPlist } = require("expo/config-plugins");
const fs = require("fs");
const path = require("path");

const NETWORK_SECURITY_CONFIG = `<?xml version="1.0" encoding="utf-8"?>
<network-security-config>
  <domain-config cleartextTrafficPermitted="true">
    <domain includeSubdomains="false">127.0.0.1</domain>
    <!-- Metro, in development builds. -->
    <domain includeSubdomains="false">localhost</domain>
    <domain includeSubdomains="false">10.0.2.2</domain>
  </domain-config>
</network-security-config>
`;

const OURS = "@xml/network_security_config";

function withSabreAndroid(config) {
  config = withDangerousMod(config, ["android", async (config) => {
    const dir = path.join(config.modRequest.platformProjectRoot, "app/src/main/res/xml");
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(path.join(dir, "network_security_config.xml"), NETWORK_SECURITY_CONFIG);
    return config;
  }]);
  return withAndroidManifest(config, (config) => {
    const app = config.modResults.manifest.application[0];
    const existing = app.$["android:networkSecurityConfig"];
    // An app with its own config would lose it silently if this replaced it.
    if (existing && existing !== OURS) {
      throw new Error(
        `@sabremaps/react-native: this app already sets android:networkSecurityConfig to ${existing}. ` +
        "Remove the plugin and add a <domain-config cleartextTrafficPermitted=\"true\"> for 127.0.0.1 " +
        "to that file instead.",
      );
    }
    app.$["android:networkSecurityConfig"] = OURS;
    return config;
  });
}

function withSabreIos(config) {
  return withInfoPlist(config, (config) => {
    const ats = config.modResults.NSAppTransportSecurity ?? {};
    ats.NSAllowsLocalNetworking = true;
    config.modResults.NSAppTransportSecurity = ats;
    return config;
  });
}

module.exports = (config) => withSabreIos(withSabreAndroid(config));
