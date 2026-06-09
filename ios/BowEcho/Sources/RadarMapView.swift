import MapKit
import SwiftUI

/// A georeferenced radar image placed on the map as an overlay.
final class RadarImageOverlay: NSObject, MKOverlay {
    let image: CGImage
    let coordinate: CLLocationCoordinate2D
    let boundingMapRect: MKMapRect

    init(frame: RadarFrame) {
        self.image = frame.image
        let center = CLLocationCoordinate2D(latitude: frame.centerLat, longitude: frame.centerLon)
        self.coordinate = center

        // Convert the radar-centered ground extents (meters) into a lat/lon box.
        let metersPerDegLat = 111_320.0
        let dLat = frame.halfHeightM / metersPerDegLat
        let cosLat = max(0.01, cos(center.latitude * .pi / 180))
        let dLon = frame.halfWidthM / (metersPerDegLat * cosLat)

        let nw = MKMapPoint(CLLocationCoordinate2D(latitude: center.latitude + dLat,
                                                   longitude: center.longitude - dLon))
        let se = MKMapPoint(CLLocationCoordinate2D(latitude: center.latitude - dLat,
                                                   longitude: center.longitude + dLon))
        self.boundingMapRect = MKMapRect(x: min(nw.x, se.x),
                                         y: min(nw.y, se.y),
                                         width: abs(se.x - nw.x),
                                         height: abs(se.y - nw.y))
        super.init()
    }

    func suggestedRegion() -> MKCoordinateRegion {
        var region = MKCoordinateRegion(boundingMapRect)
        region.span.latitudeDelta *= 1.15
        region.span.longitudeDelta *= 1.15
        return region
    }
}

final class RadarImageOverlayRenderer: MKOverlayRenderer {
    override func draw(_ mapRect: MKMapRect, zoomScale: MKZoomScale, in context: CGContext) {
        guard let radar = overlay as? RadarImageOverlay else { return }
        let rect = self.rect(for: radar.boundingMapRect)
        context.saveGState()
        // CGImage's origin is top-left; the renderer's context is bottom-left → flip Y.
        context.translateBy(x: rect.minX, y: rect.maxY)
        context.scaleBy(x: 1, y: -1)
        context.interpolationQuality = .high
        context.draw(radar.image, in: CGRect(x: 0, y: 0, width: rect.width, height: rect.height))
        context.restoreGState()
    }
}

/// Hosts an `MKMapView` and keeps a single radar overlay in sync with `frame`.
struct RadarMapView: UIViewRepresentable {
    var frame: RadarFrame?

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> MKMapView {
        let map = MKMapView()
        map.delegate = context.coordinator
        let cfg = MKStandardMapConfiguration(elevationStyle: .flat, emphasisStyle: .muted)
        cfg.pointOfInterestFilter = .excludingAll
        map.preferredConfiguration = cfg
        map.showsCompass = true
        map.showsScale = true
        return map
    }

    func updateUIView(_ map: MKMapView, context: Context) {
        guard let frame else { return }
        // Same volume already drawn — nothing to do.
        if context.coordinator.shownTime == frame.time, !map.overlays.isEmpty { return }
        context.coordinator.shownTime = frame.time

        map.removeOverlays(map.overlays)
        let overlay = RadarImageOverlay(frame: frame)
        map.addOverlay(overlay, level: .aboveRoads)

        // Recenter only when the radar site itself changes.
        if context.coordinator.centerChanged(overlay.coordinate) {
            map.setRegion(overlay.suggestedRegion(), animated: true)
        }
    }

    final class Coordinator: NSObject, MKMapViewDelegate {
        var shownTime: Date?
        private var lastCenter: CLLocationCoordinate2D?

        func centerChanged(_ c: CLLocationCoordinate2D) -> Bool {
            defer { lastCenter = c }
            guard let l = lastCenter else { return true }
            return abs(l.latitude - c.latitude) > 0.01 || abs(l.longitude - c.longitude) > 0.01
        }

        func mapView(_ mapView: MKMapView, rendererFor overlay: MKOverlay) -> MKOverlayRenderer {
            if let radar = overlay as? RadarImageOverlay {
                return RadarImageOverlayRenderer(overlay: radar)
            }
            return MKOverlayRenderer(overlay: overlay)
        }
    }
}
