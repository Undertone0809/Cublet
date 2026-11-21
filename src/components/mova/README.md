# Mova control adaptations

Source: published `@mova-ui/react` **0.4.8**, `registry/category-select.json`
(`components/css-animations/yui-components/YuiUiKit1.tsx`) and
`registry/context-menu.json` (`components/css-animations/yui-components/YuiUiKit2.tsx`).
Public previews: https://mova-ui.com/components/buttons/yui-category-select and
https://mova-ui.com/components/buttons/yui-context-menu.

`../ui/native-select.tsx` and `../../mova-controls.css` adapt the bordered surface,
selected/hover treatment, rotating chevron, and 200ms opening transition to this
app's compact 30px controls and existing light/dark design tokens. This is an
adaptation, not a verbatim copy of the demo. The existing Base UI Select supplies
listbox semantics, keyboard navigation, typeahead, positioning and focus return;
no runtime dependency was added. Reduced motion disables the transition.

The legacy component name and `<option>` children remain supported; callers now
use `onValueChange(value)` because the trigger is an accessible custom select.
The upstream MIT and third-party license notices are included alongside this file.

The animated segmented control (`../ui/animated-segmented-control.tsx`) is adapted
from the public **0.4.11** `install-catalog/pill-tabs.json` / `AnimatedToggle`
`pill-tabs` case in the canonical `mova-library` repository. It retains Mova's
shared `layoutId` indicator and spring (`stiffness: 500`, `damping: 30`), using the
app's existing `framer-motion` runtime. The local adaptation adds isolated layout
IDs, controlled values, disabled items, a named button group with `aria-pressed`,
roving keyboard focus, Left/Right/Home/End navigation, and reduced-motion support.
It changes selection only on interaction, so scanner polling cannot replay it.
The existing `../ui/tabs.tsx` uses the same indicator inside Base UI tabs while
retaining its tab/tabpanel semantics and default keyboard behavior.

The shared checkbox uses the same check path and 200ms stroke draw from public
`install-catalog/checkbox-draw.json` / `AnimatedFormElement`. Base UI continues to
own selection, form semantics and the indeterminate state; a mounted indicator
supports the reverse transition. The shared button's inset hover/press surface
is adapted from public `install-catalog/inset-action-button.json` (Rudder,
Apache-2.0; its notice is in `LICENSE.third-party`). Both use existing Mova and
macOS semantic tokens and honor reduced motion. Upstream demo labels, haptics,
and internal component implementation details are not exposed in product copy.

Dialog and tooltip portal transitions use Base UI's `data-starting-style` and
`data-ending-style` lifecycle. Their close transitions finish before unmount;
reduced motion disables transitions without leaving an invisible portal mounted.
