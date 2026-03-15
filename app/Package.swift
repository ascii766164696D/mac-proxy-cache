// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "MacProxyCache",
    platforms: [
        .macOS(.v13)
    ],
    targets: [
        .executableTarget(
            name: "MacProxyCache",
            path: "MacProxyCache",
            exclude: ["Info.plist"]
        )
    ]
)
