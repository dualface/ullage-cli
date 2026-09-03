// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "UllageMac",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "UllageKit", targets: ["UllageKit"]),
        .executable(name: "UllageMac", targets: ["UllageMac"]),
        .executable(name: "UllageDaemonHelper", targets: ["UllageDaemonHelper"]),
    ],
    targets: [
        .target(name: "UllageKit", resources: [.process("Resources")]),
        .executableTarget(name: "UllageMac", dependencies: ["UllageKit"]),
        .executableTarget(name: "UllageDaemonHelper"),
        .testTarget(
            name: "UllageKitTests",
            dependencies: ["UllageKit"]
        ),
        .testTarget(name: "UllageMacTests", dependencies: ["UllageMac"]),
    ]
)
