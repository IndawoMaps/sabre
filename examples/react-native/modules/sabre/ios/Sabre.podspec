require 'json'

package = JSON.parse(File.read(File.join(__dir__, '..', 'package.json')))

Pod::Spec.new do |s|
  s.name           = 'Sabre'
  s.version        = package['version']
  s.summary        = package['description']
  s.description    = package['description']
  s.license        = { type: 'FSL', file: '../../../../../LICENSE.md' }
  s.author         = 'sabre'
  s.homepage       = 'https://github.com/IndawoMaps/sabre'
  s.platforms      = { :ios => '16.4' }
  s.swift_version  = '5.9'
  s.source         = { git: 'https://github.com/IndawoMaps/sabre.git' }
  s.static_framework = true

  s.dependency 'ExpoModulesCore'

  s.source_files = "*.swift"
  # Built by `just mobile-ios` from crates/mobile: the static library for
  # device and simulator, with sabre.h and a module map declaring SabreFFI.
  s.vendored_frameworks = 'SabreFFI.xcframework'
  s.pod_target_xcconfig = {
    'DEFINES_MODULE' => 'YES',
    'SWIFT_COMPILATION_MODE' => 'wholemodule'
  }
end
