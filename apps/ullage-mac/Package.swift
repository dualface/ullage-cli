// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "UllageMac",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "UllageKit", targets: ["UllageKit"]),
        .executable(name: "UllageMac", targets: ["UllageMac"]),
    ],
    targets: [
        .target(name: "UllageKit"),
        .executableTarget(name: "UllageMac", dependencies: ["UllageKit"]),
        .testTarget(
            name: "UllageKitTests",
            dependencies: ["UllageKit"],
            resources: [.copy("Fixtures")]
        ),
    ]
)
