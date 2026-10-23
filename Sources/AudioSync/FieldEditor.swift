import AppKit

enum FieldEditor {
    /// The field `editor` is typing for: AppKit makes it the editor's delegate. The field doesn't
    /// declare NSTextViewDelegate, so a direct cast to a final field class is folded to nil.
    @MainActor
    static func field<Field: NSTextField>(_ type: Field.Type, editedBy editor: NSTextView) -> Field? {
        (editor.delegate as AnyObject?) as? Field
    }
}
