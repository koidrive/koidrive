// SPDX-License-Identifier: Apache-2.0

import SwiftUI

struct LoadingState: View {
    let title: LocalizedStringKey

    var body: some View {
        VStack(spacing: 12) {
            ProgressView()
                .controlSize(.small)
            Text(title)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Color.text)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct HistoryBar<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: 10) {
            content
        }
        .padding(.leading, 20)
        .padding(.trailing, 16)
        .frame(height: 56)
        .overlay(alignment: .bottom) { Color.line.frame(height: 1) }
    }
}

struct HistoryFooter<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: 8) {
            content
        }
        .padding(.horizontal, 20)
        .frame(height: 56)
        .overlay(alignment: .top) { Color.line.frame(height: 1) }
    }
}

struct HistoryList<ID: Hashable, Content: View>: View {
    @Binding var selection: ID?
    let ids: [ID]
    @ViewBuilder let content: Content

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    content
                }
                .padding(12)
            }
            .focusable()
            .focusEffectDisabled()
            .onMoveCommand { direction in
                guard !ids.isEmpty else { return }
                let index = selection.flatMap(ids.firstIndex(of:))
                switch (direction, index) {
                case (.up, let index?): selection = ids[max(index - 1, 0)]
                case (.down, let index?): selection = ids[min(index + 1, ids.count - 1)]
                case (.up, nil), (.down, nil): selection = ids.first
                default: break
                }
            }
            .onChange(of: selection) { _, id in
                if let id { proxy.scrollTo(id) }
            }
        }
    }
}

struct GroupGap: View {
    var body: some View {
        Color.clear.frame(height: 6)
    }
}

extension View {
    func selectedRow(_ selected: Bool) -> some View {
        background {
            if selected {
                RoundedRectangle(cornerRadius: 6)
                    .fill(Color.koi.opacity(0.08))
                    .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color.koi, lineWidth: 1.5))
            }
        }
        .contentShape(Rectangle())
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    func trailingColumn(_ width: CGFloat) -> some View {
        lineLimit(1)
            .frame(width: width, alignment: .trailing)
    }
}
