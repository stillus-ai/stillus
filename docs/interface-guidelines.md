# Interface guidelines

These rules apply to every Stillus screen and shared native UI component.

## Action buttons

Every action button starts with an icon. Choose its presentation by where
the action is used, independently of its primary, secondary or danger tone.

| Context | Presentation | Shared component |
| --- | --- | --- |
| Form actions, including inline editing, settings and chat composition | Icon **and localized action text**, for every action | `form_action_button` |
| Dialog actions | Icon **and localized action text**, for every action | `form_action_button`, `dialog_button`, `password_dialog_button` |
| Toolbar: standard action with a commonly understood icon | Icon only | `toolbar_action_button` with a standard `ButtonAction` |
| Toolbar: custom action without an unambiguous conventional icon | Icon **and localized action text** | `toolbar_action_button` with `ButtonAction::Custom` |

Save, Cancel, Delete, Paste, Send and Stop must have visible captions when
they act on a form. An inline editing row below a toolbar is still a form.
Disabled and busy form actions retain their captions. Never hide a caption
just to fit a narrow window or use `Custom` merely to force a standard form
action to display text.

Copy, Add, Edit, Search, Refresh, Pin, Favorite and Delete are examples of
standard toolbar actions. A familiar icon must match the actual operation:
opening the AI request journal, restoring unsaved work and loading the disk
version need explanatory text. A generic document or recovery icon alone
does not explain these operations. The request journal is accessible only
from AI settings; do not add it to chat or document toolbars. Menus and
expanded navigation retain their labels.

Input accessories (show/hide password, clear search, remove a selected tag)
and compact navigation controls are distinct from form action buttons.
They may use conventional icons inside their control, with localized hints.
Do not use accessory components for submitting, cancelling or deleting a form.

Every icon-only button MUST have a nonempty localized hover tooltip,
including unavailable buttons. Use the shared button's title argument and
update it with the action state. Preserve keyboard-focus hints too. Use the
component's enabled predicate for unavailable buttons: Floem `.disabled()`
on an icon-only button or its wrapper suppresses hover dispatch.

## Shared controls and styles

Build controls through `app/stillus/src/ui/`: buttons, inputs, textareas,
selects, secret-input surfaces, menus, tooltips and modal shells. Fix
interaction and appearance in the component, never in a per-screen copy.
Components receive values and callbacks, not application controllers.

Use scoped UI styles. Do not apply global theme/style overrides or deep
selectors for local changes. Reuse the shared typography, spacing and color
tokens and the `actions`, `settings_card` and `toolbar_edit_bar` components.

## Forms and layout

Keep action captions readable at the minimum supported window size
(960 × 600) and in every locale. Size labeled buttons to their content;
do not constrain them to an icon-sized square. Wrap action groups before
buttons overlap, shrink or escape their card. Inputs should shrink within
the available space.
Check long translations and right-to-left layouts after changing controls.

Use `TextArea` for form multiline input. Never insert placeholders into its
document or implement a screen-specific caret/blink workaround. The bounded
Markdown document editor and OS-owned dialogs are specialized exceptions;
they must not become alternative form-control implementations.

Keep focus visible and keyboard operations consistent across screens.
Associate validation feedback with the affected field. Selects, menus and
tooltips must stay within the window and follow their anchors on resize.
Preserve the secret-input component's clipboard and plaintext protections.

## Verification

Write or update tests for behavior changes. Check the shared presentation
rules as well as the affected product forms and toolbars, including busy
and disabled states. Keep `make audit-ui-components` and
`make ui-click-components` passing. Run fast unit tests first, then the UI
scenarios related to the change, following the check policy in `AGENTS.md`.
