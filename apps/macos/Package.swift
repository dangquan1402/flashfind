// swift-tools-version:6.0
// FlashFind demo app: links the Rust core in-process through the C ABI
// (crates/flashfind-ffi). Build with scripts/build-app.sh, which builds
// libflashfind.a first and passes its folder to the linker.
import PackageDescription

let package = Package(
    name: "FlashFind",
    platforms: [.macOS(.v15)],
    targets: [
        .systemLibrary(name: "CFlashFind", path: "Sources/CFlashFind"),
        .executableTarget(
            name: "FlashFind",
            dependencies: ["CFlashFind"],
            path: "Sources/FlashFind",
            linkerSettings: [
                .linkedFramework("CoreServices"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("Carbon"),
                .linkedFramework("Quartz"),
            ]
        ),
    ],
    swiftLanguageModes: [.v5]
)
