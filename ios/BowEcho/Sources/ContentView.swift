import SwiftUI

struct ContentView: View {
    @StateObject private var vm = RadarViewModel()

    var body: some View {
        ZStack(alignment: .top) {
            RadarMapView(frame: vm.frame)
                .ignoresSafeArea()

            VStack(spacing: 10) {
                controlBar
                if let err = vm.errorMessage {
                    errorBanner(err)
                }
                Spacer()
                bottomBar
            }
            .padding(.horizontal, 12)
            .padding(.top, 6)
        }
        .onAppear { if vm.frame == nil { vm.refresh() } }
        .onChange(of: vm.product) { _, _ in vm.refresh() }
        .onChange(of: vm.site) { _, _ in vm.refresh() }
    }

    // MARK: Top control bar — site + product

    private var controlBar: some View {
        HStack(spacing: 10) {
            Menu {
                ForEach(RadarSiteOption.presets) { option in
                    Button {
                        vm.site = option
                    } label: {
                        Text("\(option.id) — \(option.name)")
                    }
                }
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: "antenna.radiowaves.left.and.right")
                    Text(vm.site.id).fontWeight(.semibold)
                    Image(systemName: "chevron.down").font(.caption2)
                }
                .padding(.horizontal, 12).padding(.vertical, 9)
                .background(.ultraThinMaterial, in: Capsule())
            }

            Picker("Product", selection: $vm.product) {
                ForEach(RadarProduct.allCases) { p in
                    Text(p.short).tag(p)
                }
            }
            .pickerStyle(.segmented)
            .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 9))
        }
        .font(.subheadline)
    }

    private func errorBanner(_ message: String) -> some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
            Text(message).font(.footnote).lineLimit(2)
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 12).padding(.vertical, 9)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.red.opacity(0.85), in: RoundedRectangle(cornerRadius: 10))
    }

    // MARK: Bottom bar — timestamp + refresh

    private var bottomBar: some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) {
                Text(vm.product.label).font(.caption).fontWeight(.semibold)
                Text(timestampText).font(.caption2).foregroundStyle(.secondary)
            }
            .padding(.horizontal, 12).padding(.vertical, 8)
            .background(.ultraThinMaterial, in: Capsule())

            Spacer()

            Button(action: { vm.refresh() }) {
                ZStack {
                    Circle().fill(.ultraThinMaterial).frame(width: 52, height: 52)
                    if vm.isLoading {
                        ProgressView()
                    } else {
                        Image(systemName: "arrow.clockwise").font(.title3.weight(.semibold))
                    }
                }
            }
            .disabled(vm.isLoading)
        }
        .padding(.bottom, 8)
    }

    private var timestampText: String {
        guard let t = vm.frame?.time else {
            return vm.isLoading ? "Loading…" : "No data yet"
        }
        let f = RelativeDateTimeFormatter()
        f.unitsStyle = .abbreviated
        return "Valid \(f.localizedString(for: t, relativeTo: Date()))"
    }
}

#Preview {
    ContentView()
}
