//! `UITextPosition` / `UITextRange` for [`WinitView`](super::view::WinitView)'s
//! `UITextInput`: positions are UTF-16 offsets into the view's text mirror
//! ([`TextMirror`](crate::platform_impl::ios_text_mirror::TextMirror)).

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{declare_class, msg_send, msg_send_id, mutability, ClassType, DeclaredClass};
use objc2_foundation::{MainThreadMarker, NSObject};
use objc2_ui_kit::{UITextPosition, UITextRange};

declare_class!(
    /// A UTF-16 offset into the mirror.
    pub(crate) struct WinitTextPosition;

    unsafe impl ClassType for WinitTextPosition {
        #[inherits(NSObject)]
        type Super = UITextPosition;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "WinitTextPosition";
    }

    impl DeclaredClass for WinitTextPosition {
        type Ivars = usize;
    }
);

impl WinitTextPosition {
    pub(crate) fn at(mtm: MainThreadMarker, offset: usize) -> Retained<UITextPosition> {
        let this = mtm.alloc().set_ivars(offset);
        let this: Retained<Self> = unsafe { msg_send_id![super(this), init] };
        Retained::into_super(this)
    }
}

declare_class!(
    /// A UTF-16 range into the mirror, `start <= end`.
    pub(crate) struct WinitTextRange;

    unsafe impl ClassType for WinitTextRange {
        #[inherits(NSObject)]
        type Super = UITextRange;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "WinitTextRange";
    }

    impl DeclaredClass for WinitTextRange {
        type Ivars = (usize, usize);
    }

    unsafe impl WinitTextRange {
        #[method_id(start)]
        fn start(&self) -> Retained<UITextPosition> {
            WinitTextPosition::at(MainThreadMarker::from(self), self.ivars().0)
        }

        #[method_id(end)]
        fn end(&self) -> Retained<UITextPosition> {
            WinitTextPosition::at(MainThreadMarker::from(self), self.ivars().1)
        }

        #[method(isEmpty)]
        fn is_empty(&self) -> bool {
            self.ivars().0 == self.ivars().1
        }
    }
);

impl WinitTextRange {
    pub(crate) fn between(mtm: MainThreadMarker, a: usize, b: usize) -> Retained<UITextRange> {
        let this = mtm.alloc().set_ivars((a.min(b), a.max(b)));
        let this: Retained<Self> = unsafe { msg_send_id![super(this), init] };
        Retained::into_super(this)
    }
}

/// The offset of a position this view handed out; `None` for any other
/// (UIKit only passes back positions it got from us).
pub(crate) fn offset_of(position: &UITextPosition) -> Option<usize> {
    let obj: &AnyObject = position.as_ref();
    let ours: bool = unsafe { msg_send![obj, isKindOfClass: WinitTextPosition::class()] };
    // SAFETY: checked to be a `WinitTextPosition` just above.
    ours.then(|| {
        *unsafe { &*(position as *const UITextPosition as *const WinitTextPosition) }.ivars()
    })
}

/// The UTF-16 bounds of a range this view handed out.
pub(crate) fn range_of(range: &UITextRange) -> Option<(usize, usize)> {
    let obj: &AnyObject = range.as_ref();
    let ours: bool = unsafe { msg_send![obj, isKindOfClass: WinitTextRange::class()] };
    // SAFETY: checked to be a `WinitTextRange` just above.
    ours.then(|| *unsafe { &*(range as *const UITextRange as *const WinitTextRange) }.ivars())
}
