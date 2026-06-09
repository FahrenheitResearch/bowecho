import Foundation

/// Radar moments, mapped to the `moment_code` the Rust engine expects.
enum RadarProduct: Int32, CaseIterable, Identifiable {
    case reflectivity = 0
    case velocity = 1
    case correlation = 4
    case differentialReflectivity = 3
    case spectrumWidth = 2

    var id: Int32 { rawValue }

    var label: String {
        switch self {
        case .reflectivity: return "Reflectivity"
        case .velocity: return "Velocity"
        case .correlation: return "Correlation"
        case .differentialReflectivity: return "ZDR"
        case .spectrumWidth: return "Spectrum Width"
        }
    }

    var short: String {
        switch self {
        case .reflectivity: return "REF"
        case .velocity: return "VEL"
        case .correlation: return "CC"
        case .differentialReflectivity: return "ZDR"
        case .spectrumWidth: return "SW"
        }
    }
}

/// A small starter set of WSR-88D sites. (Full discovery via the engine later.)
struct RadarSiteOption: Identifiable, Hashable {
    let id: String       // ICAO, e.g. "KTLX"
    let name: String

    static let presets: [RadarSiteOption] = [
        .init(id: "KTLX", name: "Oklahoma City, OK"),
        .init(id: "KFWS", name: "Dallas / Fort Worth, TX"),
        .init(id: "KEAX", name: "Kansas City, MO"),
        .init(id: "KLOT", name: "Chicago, IL"),
        .init(id: "KMPX", name: "Minneapolis, MN"),
        .init(id: "KFFC", name: "Atlanta, GA"),
        .init(id: "KOKX", name: "New York City, NY"),
        .init(id: "KMLB", name: "Melbourne, FL"),
        .init(id: "KFTG", name: "Denver, CO"),
        .init(id: "KMUX", name: "San Francisco Bay, CA"),
    ]
}

@MainActor
final class RadarViewModel: ObservableObject {
    @Published var site: RadarSiteOption = RadarSiteOption.presets[0]
    @Published var product: RadarProduct = .reflectivity
    @Published var frame: RadarFrame?
    @Published var isLoading = false
    @Published var errorMessage: String?
    @Published var lastUpdated: Date?

    func refresh() {
        guard !isLoading else { return }
        isLoading = true
        errorMessage = nil
        let siteID = site.id
        let code = product.rawValue
        Task {
            do {
                let frame = try await Self.render(site: siteID, code: code)
                self.frame = frame
                self.lastUpdated = Date()
            } catch {
                self.errorMessage = (error as? RadarError)?.errorDescription ?? error.localizedDescription
            }
            self.isLoading = false
        }
    }

    /// Run the blocking engine call on a background queue.
    private static func render(site: String, code: Int32) async throws -> RadarFrame {
        try await withCheckedThrowingContinuation { continuation in
            DispatchQueue.global(qos: .userInitiated).async {
                do {
                    continuation.resume(returning: try RadarEngine.renderLatest(site: site, momentCode: code))
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }
}
