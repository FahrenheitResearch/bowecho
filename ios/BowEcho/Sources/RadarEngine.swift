import CoreGraphics
import Foundation

/// A rendered radar frame ready to drop onto the map.
struct RadarFrame {
    let image: CGImage
    let centerLat: Double
    let centerLon: Double
    let halfWidthM: Double
    let halfHeightM: Double
    let time: Date
}

enum RadarError: LocalizedError {
    case engine(String)
    case image

    var errorDescription: String? {
        switch self {
        case .engine(let m): return m
        case .image: return "Could not build an image from the radar data."
        }
    }
}

/// Thin Swift wrapper over the Rust `bowecho_ffi` C ABI.
enum RadarEngine {
    private static func cacheDirectory() -> String {
        let base = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
        let dir = base.appendingPathComponent("nexrad", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.path
    }

    /// Fetch + decode + render the latest volume for `site`. BLOCKS on network —
    /// must be called off the main thread.
    static func renderLatest(site: String, momentCode: Int32, sizePx: UInt32 = 2048) throws -> RadarFrame {
        let cacheDir = cacheDirectory()
        var out = BowEchoRender()
        let rc = site.withCString { sitePtr in
            cacheDir.withCString { cachePtr in
                bowecho_render_latest(sitePtr, momentCode, sizePx, cachePtr, &out)
            }
        }

        guard rc == 0 else {
            let msg = bowecho_last_error().map { String(cString: $0) } ?? "engine error \(rc)"
            throw RadarError.engine(msg)
        }
        defer { bowecho_render_free(&out) }

        guard let rgba = out.rgba, out.len > 0 else { throw RadarError.image }
        let width = Int(out.width)
        let height = Int(out.height)
        // Copy the pixels out before the Rust buffer is freed.
        let data = Data(bytes: rgba, count: Int(out.len))
        guard let provider = CGDataProvider(data: data as CFData) else { throw RadarError.image }

        guard let cg = CGImage(
            width: width,
            height: height,
            bitsPerComponent: 8,
            bitsPerPixel: 32,
            bytesPerRow: width * 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
            provider: provider,
            decode: nil,
            shouldInterpolate: true,
            intent: .defaultIntent
        ) else { throw RadarError.image }

        return RadarFrame(
            image: cg,
            centerLat: out.center_lat,
            centerLon: out.center_lon,
            halfWidthM: out.half_width_m,
            halfHeightM: out.half_height_m,
            time: Date(timeIntervalSince1970: TimeInterval(out.volume_time_unix))
        )
    }
}
