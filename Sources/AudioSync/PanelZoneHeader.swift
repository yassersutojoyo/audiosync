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
    private var waitNote: String? { model.waitNote(of: zone) }
    // A waiting zone's icon is outlined, as while streaming is off; a click still switches it off.
    private var shownState: SpeakerState { waitNote == nil ? state : .willPlay }
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
                SpeakerBadge(symbol: MenuBarView.zoneSymbol, state: shownState, size: 26, ring: true)
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
            .accessibilityHint(t.rowHint(state, note: waitNote))
            .help(t.help(state))
            .onKeyPress(keys: [.leftArrow, .rightArrow]) { arrowKey($0) }

            Button { hit(.name) } label: {
                HStack(spacing: 0) {
                    ScrollingTitle(text: zone.name)
                        .font(.body)
                        .opacity(nonePresent ? 0.45 : 1)
                        .layoutPriority(1)
                    Spacer(minLength: 4)
                    // Placed and styled as a panel row's note; VoiceOver reads it in the icon's hint.
                    if let waitNote {
                        Text(waitNote)
                            .font(.subheadline)
                            .foregroundStyle(Brand.secondaryText)
                            .lineLimit(1)
                            .help(waitNote)
                    }
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
            .onKeyPress(keys: [.leftArrow, .rightArrow]) { arrowKey($0) }
        }
        .frame(height: height)
        .background(RoundedRectangle(cornerRadius: BrandTokens.Radius.control)
            .fill(hover.isHovered ? Color.primary.opacity(0.07) : Color.clear))
        .onHover { hover.isHovered = $0 }
    }

    // The controller animates the fold, so the panel resizes with the rows.
    private func hit(_ area: ZoneHeaderHit.Area) {
        switch ZoneHeaderHit.action(for: area, nonePresent: nonePresent) {
        case .fold: fold()
        case .toggleZone: model.actions.toggleZone(zone.id)
        case .none: break
        }
    }

    private func fold() { model.foldPanelZone { disclosure.toggle(zone.id) } }

    private func arrowKey(_ press: KeyPress) -> KeyPress.Result {
        guard ZoneHeaderHit.arrowFolds(right: press.key == .rightArrow, expanded: expanded) else { return .ignored }
        fold()
        return .handled
    }
}
