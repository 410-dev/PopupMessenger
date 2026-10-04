// swift-tools-version:5.5

import PackageDescription

let package = Package(
  name: "tauri-plugin-pm-native",
  platforms: [
    .iOS(.v15)
  ],
  products: [
    .library(
      name: "tauri-plugin-pm-native",
      type: .static,
      targets: ["tauri-plugin-pm-native"])
  ],
  dependencies: [
    .package(name: "Tauri", path: "../.tauri/tauri-api")
  ],
  targets: [
    .target(
      name: "tauri-plugin-pm-native",
      dependencies: [
        .byName(name: "Tauri")
      ],
      path: "Sources")
  ]
)
