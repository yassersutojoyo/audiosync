import SwiftUI

/// A zone's row in the menu bar panel. Routed like the main window's zone
/// header: only the round icon switches the zone, and the name, the arrow and
/// the space between them show or hide the zone's speakers.
struct PanelZoneHeader: View {
    @ObservedObject var model: AppModel
    @ObservedObject var disclosure: ZoneDisclosure
    let zone: AppModel.Zone
    let height: CGFloat
    @StateObject private var hover = HoverState()

    private var state: SpeakerState { model.state(of: zone) }
    private var expanded: Bool { disclosure.isExpanded(zone.id) }
    // Only the icon is disabled then: unfolding still lists the missing speakers.
    private var nonePresent: Bool { zone.present == 0 }

    var body: some View {
        let t = model.text
        let foldTitle = expanded ? t.hideZoneSpeakers(zone.name) : t.showZoneSpeakers(zone.name)
        HStack(spacing: 9) {
            Button { hit(.zoneIcon) } label: {
                SpeakerBadge(symbol: MenuBarView.zoneSymbol, state: state, size: 24)
                    .contentShape(Circle())
            }
            .buttonStyle(.plain)
            .disabled(nonePresent)
            .opacity(nonePresent ? 0.45 : 1)
            .accessibilityLabel(t.zoneLabel(zone.name))
            .accessibilityValue(nonePresent ? model.countText(of: zone)
                                : t.zoneValue(on: zone.on, of: zone.total, waiting: zone.waiting))
            .accessibilityHint(t.rowHint(state, note: nil))
            .help(t.help(state))

            Button { hit(.name) } label: {
                HStack(spacing: 4) {
                    Text(zone.name)
                        .font(.callout.weight(state == .playing ? .semibold : .regular))
                        .lineLimit(1)
                        .opacity(nonePresent ? 0.45 : 1)
                    Spacer(minLength: 4)
                    Image(systemName: "chevron.right")
                        .font(.caption.weight(.semibold))
                        .rotationEffect(.degrees(expanded ? 90 : 0))
                        .foregroundStyle(Brand.secondaryText)
                }
                .frame(maxHeight: .infinity)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(foldTitle)
            .help(foldTitle)
        }
        .padding(.horizontal, 6)
        .frame(height: height)
        .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.control)
            .fill(hover.isHovered ? Color.primary.opacity(0.07) : Color.clear))
        .onHover { hover.isHovered = $0 }
    }

    // No animation: the popover resizes to the new height on its own.
    private func hit(_ area: ZoneHeaderHit.Area) {
        switch ZoneHeaderHit.action(for: area) {
        case .fold: disclosure.toggle(zone.id)
        case .toggleZone: model.actions.toggleZone(zone.id)
        case .none: break
        }
    }
}
