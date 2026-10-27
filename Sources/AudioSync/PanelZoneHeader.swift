import SwiftUI

/// A zone's row in the menu bar panel. Routed like the main window's zone
/// header: only the round icon switches the zone; the arrow, the name and the
/// empty space around them show or hide the zone's speakers.
struct PanelZoneHeader: View {
    @ObservedObject var model: AppModel
    @ObservedObject var disclosure: ZoneDisclosure
    let zone: AppModel.Zone
    let height: CGFloat
    @StateObject private var hover = HoverState()

    private var state: SpeakerState { model.state(of: zone) }
    private var expanded: Bool { disclosure.isExpanded(zone.id) }
    // The icon greys out and folds then: unfolding still lists the missing speakers.
    private var nonePresent: Bool { zone.present == 0 }

    var body: some View {
        let t = model.text
        let foldTitle = expanded ? t.hideZoneSpeakers(zone.name) : t.showZoneSpeakers(zone.name)
        // No spacing: each gap is padding inside a fold button, so the empty
        // space folds like the main window's blank area and only the icon
        // switches the zone.
        HStack(spacing: 0) {
            Button { hit(.chevron) } label: {
                Image(systemName: "chevron.right")
                    .font(.caption.weight(.semibold))
                    .rotationEffect(.degrees(expanded ? 90 : 0))
                    .frame(width: 12)
                    .padding(.leading, 6)
                    .padding(.trailing, 5)
                    .frame(maxHeight: .infinity)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .foregroundStyle(Brand.secondaryText)
            // The name button carries the same action and label.
            .accessibilityHidden(true)
            .focusable(false)
            .help(foldTitle)

            Button { hit(.zoneIcon) } label: {
                SpeakerBadge(symbol: MenuBarView.zoneSymbol, state: state, size: 24)
                    .contentShape(Circle())
            }
            .buttonStyle(.plain)
            .opacity(nonePresent ? 0.45 : 1)
            // With nothing to switch, the name button speaks for the row.
            .accessibilityHidden(nonePresent)
            .focusable(!nonePresent)
            .accessibilityLabel(t.zoneLabel(zone.name))
            .accessibilityValue(nonePresent ? model.countText(of: zone)
                                : t.zoneValue(on: zone.on, of: zone.total, waiting: zone.waiting))
            .accessibilityHint(t.rowHint(state, note: nil))
            .help(t.help(state))

            Button { hit(.name) } label: {
                HStack(spacing: 0) {
                    Text(zone.name)
                        .font(.callout.weight(state == .playing ? .semibold : .regular))
                        .lineLimit(1)
                        .opacity(nonePresent ? 0.45 : 1)
                    Spacer(minLength: 4)
                }
                .padding(.leading, 9)
                .padding(.trailing, 6)
                .frame(maxHeight: .infinity)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(foldTitle)
            .accessibilityValue(nonePresent ? model.countText(of: zone) : "")
            .help(foldTitle)
        }
        .frame(height: height)
        .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.control)
            .fill(hover.isHovered ? Color.primary.opacity(0.07) : Color.clear))
        .onHover { hover.isHovered = $0 }
    }

    // No animation: the popover resizes to the new height on its own.
    private func hit(_ area: ZoneHeaderHit.Area) {
        switch ZoneHeaderHit.action(for: area, nonePresent: nonePresent) {
        case .fold: disclosure.toggle(zone.id)
        case .toggleZone: model.actions.toggleZone(zone.id)
        case .none: break
        }
    }
}
