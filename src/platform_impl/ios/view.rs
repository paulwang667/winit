#![allow(clippy::unnecessary_cast)]
use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2::{declare_class, msg_send, msg_send_id, mutability, sel, ClassType, DeclaredClass};
use objc2_foundation::{
    CGFloat, CGPoint, CGRect, CGSize, MainThreadMarker, NSArray, NSInteger, NSObject, NSRange,
    NSSet, NSString,
};
use objc2_ui_kit::{
    UICoordinateSpace, UIEvent, UIForceTouchCapability, UIGestureRecognizer,
    UIGestureRecognizerDelegate, UIGestureRecognizerState, UIKeyInput, UIPanGestureRecognizer,
    UIPinchGestureRecognizer, UIResponder, UIRotationGestureRecognizer, UITapGestureRecognizer,
    UITextInput, UITextInputStringTokenizer, UITextInputTraits, UITextPosition, UITextRange,
    UITouch, UITouchPhase, UITouchType, UITraitEnvironment, UIView,
};

use super::app_state::{self, EventWrapper};
use super::text_input::{offset_of, range_of, WinitTextPosition, WinitTextRange};
use super::window::WinitUIWindow;
use crate::dpi::PhysicalPosition;
use crate::event::{
    ElementState, Event, Force, Ime, KeyEvent, MouseScrollDelta, Touch, TouchPhase, WindowEvent,
};
use crate::keyboard::{Key, KeyCode, KeyLocation, NamedKey, NativeKeyCode, PhysicalKey};
use crate::platform::ios::{KeyboardType, ReturnKeyType, TextInputState, TextInputTraits};
use crate::platform_impl::ios_text_mirror::{InsertAction, TextMirror};
use crate::platform_impl::platform::DEVICE_ID;
use crate::platform_impl::KeyEventExtra;
use crate::window::{WindowAttributes, WindowId as RootWindowId};

pub struct WinitViewState {
    pinch_gesture_recognizer: RefCell<Option<Retained<UIPinchGestureRecognizer>>>,
    doubletap_gesture_recognizer: RefCell<Option<Retained<UITapGestureRecognizer>>>,
    rotation_gesture_recognizer: RefCell<Option<Retained<UIRotationGestureRecognizer>>>,
    pan_gesture_recognizer: RefCell<Option<Retained<UIPanGestureRecognizer>>>,

    // for iOS delta references the start of the Gesture
    rotation_last_delta: Cell<CGFloat>,
    pinch_last_delta: Cell<CGFloat>,
    pan_last_delta: Cell<CGPoint>,

    /// The focused field the app handed over
    /// (`WindowExtIOS::set_text_input_state`); `None` keeps the plain
    /// `UIKeyInput` behaviour.
    text: RefCell<Option<TextMirror>>,
    traits: Cell<TextInputTraits>,
    /// UIKit's `inputDelegate` (a `UITextInputDelegate`).
    input_delegate: RefCell<Option<Retained<AnyObject>>>,
    tokenizer: RefCell<Option<Retained<UITextInputStringTokenizer>>>,
    /// The caret's rect in view points (`Window::set_ime_cursor_area`).
    cursor_area: Cell<CGRect>,

    /// How much of the window the soft keyboard covers, in points; folded
    /// into the safe area the window reports.
    keyboard_overlap: Cell<CGFloat>,
    keyboard_observers: RefCell<Vec<Retained<NSObject>>>,
    /// Trackpad and mouse-wheel scrolling (indirect touches only).
    scroll_gesture_recognizer: RefCell<Option<Retained<UIPanGestureRecognizer>>>,
    scroll_last: Cell<CGPoint>,
}

declare_class!(
    pub(crate) struct WinitView;

    unsafe impl ClassType for WinitView {
        #[inherits(UIResponder, NSObject)]
        type Super = UIView;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "WinitUIView";
    }

    impl DeclaredClass for WinitView {
        type Ivars = WinitViewState;
    }

    unsafe impl WinitView {
        #[method(drawRect:)]
        fn draw_rect(&self, rect: CGRect) {
            let mtm = MainThreadMarker::new().unwrap();
            let window = self.window().unwrap();
            app_state::handle_nonuser_event(
                mtm,
                EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id: RootWindowId(window.id()),
                    event: WindowEvent::RedrawRequested,
                }),
            );
            let _: () = unsafe { msg_send![super(self), drawRect: rect] };
        }

        #[method(layoutSubviews)]
        fn layout_subviews(&self) {
            let mtm = MainThreadMarker::new().unwrap();
            let _: () = unsafe { msg_send![super(self), layoutSubviews] };

            let window = self.window().unwrap();
            let window_bounds = window.bounds();
            let screen = window.screen();
            let screen_space = screen.coordinateSpace();
            let screen_frame = self.convertRect_toCoordinateSpace(window_bounds, &screen_space);
            let scale_factor = screen.scale();
            let size = crate::dpi::LogicalSize {
                width: screen_frame.size.width as f64,
                height: screen_frame.size.height as f64,
            }
            .to_physical(scale_factor as f64);

            // If the app is started in landscape, the view frame and window bounds can be mismatched.
            // The view frame will be in portrait and the window bounds in landscape. So apply the
            // window bounds to the view frame to make it consistent.
            let view_frame = self.frame();
            if view_frame != window_bounds {
                self.setFrame(window_bounds);
            }

            app_state::handle_nonuser_event(
                mtm,
                EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id: RootWindowId(window.id()),
                    event: WindowEvent::Resized(size),
                }),
            );
        }

        #[method(setContentScaleFactor:)]
        fn set_content_scale_factor(&self, untrusted_scale_factor: CGFloat) {
            let mtm = MainThreadMarker::new().unwrap();
            let _: () =
                unsafe { msg_send![super(self), setContentScaleFactor: untrusted_scale_factor] };

            // `window` is null when `setContentScaleFactor` is invoked prior to `[UIWindow
            // makeKeyAndVisible]` at window creation time (either manually or internally by
            // UIKit when the `UIView` is first created), in which case we send no events here
            let window = match self.window() {
                Some(window) => window,
                None => return,
            };
            // `setContentScaleFactor` may be called with a value of 0, which means "reset the
            // content scale factor to a device-specific default value", so we can't use the
            // parameter here. We can query the actual factor using the getter
            let scale_factor = self.contentScaleFactor();
            assert!(
                !scale_factor.is_nan()
                    && scale_factor.is_finite()
                    && scale_factor.is_sign_positive()
                    && scale_factor > 0.0,
                "invalid scale_factor set on UIView",
            );
            let scale_factor = scale_factor as f64;
            let bounds = self.bounds();
            let screen = window.screen();
            let screen_space = screen.coordinateSpace();
            let screen_frame = self.convertRect_toCoordinateSpace(bounds, &screen_space);
            let size = crate::dpi::LogicalSize {
                width: screen_frame.size.width as f64,
                height: screen_frame.size.height as f64,
            };
            let window_id = RootWindowId(window.id());
            app_state::handle_nonuser_events(
                mtm,
                std::iter::once(EventWrapper::ScaleFactorChanged(
                    app_state::ScaleFactorChanged {
                        window,
                        scale_factor,
                        suggested_size: size.to_physical(scale_factor),
                    },
                ))
                .chain(std::iter::once(EventWrapper::StaticEvent(
                    Event::WindowEvent {
                        window_id,
                        event: WindowEvent::Resized(size.to_physical(scale_factor)),
                    },
                ))),
            );
        }

        #[method(touchesBegan:withEvent:)]
        fn touches_began(&self, touches: &NSSet<UITouch>, _event: Option<&UIEvent>) {
            self.handle_touches(touches)
        }

        #[method(touchesMoved:withEvent:)]
        fn touches_moved(&self, touches: &NSSet<UITouch>, _event: Option<&UIEvent>) {
            self.handle_touches(touches)
        }

        #[method(touchesEnded:withEvent:)]
        fn touches_ended(&self, touches: &NSSet<UITouch>, _event: Option<&UIEvent>) {
            self.handle_touches(touches)
        }

        #[method(touchesCancelled:withEvent:)]
        fn touches_cancelled(&self, touches: &NSSet<UITouch>, _event: Option<&UIEvent>) {
            self.handle_touches(touches)
        }

        #[method(pinchGesture:)]
        fn pinch_gesture(&self, recognizer: &UIPinchGestureRecognizer) {
            let window = self.window().unwrap();

            let (phase, delta) = match recognizer.state() {
                UIGestureRecognizerState::Began => {
                    self.ivars().pinch_last_delta.set(recognizer.scale());
                    (TouchPhase::Started, 0.0)
                }
                UIGestureRecognizerState::Changed => {
                    let last_scale: f64 = self.ivars().pinch_last_delta.replace(recognizer.scale());
                    (TouchPhase::Moved, recognizer.scale() - last_scale)
                }
                UIGestureRecognizerState::Ended => {
                    let last_scale: f64 = self.ivars().pinch_last_delta.replace(0.0);
                    (TouchPhase::Moved, recognizer.scale() - last_scale)
                }
                UIGestureRecognizerState::Cancelled | UIGestureRecognizerState::Failed => {
                    self.ivars().rotation_last_delta.set(0.0);
                    // Pass -delta so that action is reversed
                    (TouchPhase::Cancelled, -recognizer.scale())
                }
                state => panic!("unexpected recognizer state: {state:?}"),
            };

            let gesture_event = EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::PinchGesture {
                    device_id: DEVICE_ID,
                    delta: delta as f64,
                    phase,
                },
            });

            let mtm = MainThreadMarker::new().unwrap();
            app_state::handle_nonuser_event(mtm, gesture_event);
        }

        #[method(doubleTapGesture:)]
        fn double_tap_gesture(&self, recognizer: &UITapGestureRecognizer) {
            let window = self.window().unwrap();

            if recognizer.state() == UIGestureRecognizerState::Ended {
                let gesture_event = EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id: RootWindowId(window.id()),
                    event: WindowEvent::DoubleTapGesture {
                        device_id: DEVICE_ID,
                    },
                });

                let mtm = MainThreadMarker::new().unwrap();
                app_state::handle_nonuser_event(mtm, gesture_event);
            }
        }

        #[method(rotationGesture:)]
        fn rotation_gesture(&self, recognizer: &UIRotationGestureRecognizer) {
            let window = self.window().unwrap();

            let (phase, delta) = match recognizer.state() {
                UIGestureRecognizerState::Began => {
                    self.ivars().rotation_last_delta.set(0.0);

                    (TouchPhase::Started, 0.0)
                }
                UIGestureRecognizerState::Changed => {
                    let last_rotation = self.ivars().rotation_last_delta.replace(recognizer.rotation());

                    (TouchPhase::Moved, recognizer.rotation() - last_rotation)
                }
                UIGestureRecognizerState::Ended => {
                    let last_rotation = self.ivars().rotation_last_delta.replace(0.0);

                    (TouchPhase::Ended, recognizer.rotation() - last_rotation)
                }
                UIGestureRecognizerState::Cancelled | UIGestureRecognizerState::Failed => {
                    self.ivars().rotation_last_delta.set(0.0);

                    // Pass -delta so that action is reversed
                    (TouchPhase::Cancelled, -recognizer.rotation())
                }
                state => panic!("unexpected recognizer state: {state:?}"),
            };

            // Make delta negative to match macos, convert to degrees
            let gesture_event = EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::RotationGesture {
                    device_id: DEVICE_ID,
                    delta: -delta.to_degrees() as _,
                    phase,
                },
            });

            let mtm = MainThreadMarker::new().unwrap();
            app_state::handle_nonuser_event(mtm, gesture_event);
        }

        #[method(panGesture:)]
        fn pan_gesture(&self, recognizer: &UIPanGestureRecognizer) {
            let window = self.window().unwrap();

            let translation = recognizer.translationInView(Some(self));

            let (phase, dx, dy) = match recognizer.state() {
                UIGestureRecognizerState::Began => {
                    self.ivars().pan_last_delta.set(translation);

                    (TouchPhase::Started, 0.0, 0.0)
                }
                UIGestureRecognizerState::Changed => {
                    let last_pan: CGPoint = self.ivars().pan_last_delta.replace(translation);

                    let dx = translation.x - last_pan.x;
                    let dy = translation.y - last_pan.y;

                    (TouchPhase::Moved, dx, dy)
                }
                UIGestureRecognizerState::Ended => {
                    let last_pan: CGPoint = self.ivars().pan_last_delta.replace(CGPoint{x:0.0, y:0.0});

                    let dx = translation.x - last_pan.x;
                    let dy = translation.y - last_pan.y;

                    (TouchPhase::Ended, dx, dy)
                }
                UIGestureRecognizerState::Cancelled | UIGestureRecognizerState::Failed => {
                    let last_pan: CGPoint = self.ivars().pan_last_delta.replace(CGPoint{x:0.0, y:0.0});

                    // Pass -delta so that action is reversed
                    (TouchPhase::Cancelled, -last_pan.x, -last_pan.y)
                }
                state => panic!("unexpected recognizer state: {state:?}"),
            };


            let gesture_event = EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::PanGesture {
                    device_id: DEVICE_ID,
                    delta: PhysicalPosition::new(dx as _, dy as _),
                    phase,
                },
            });

            let mtm = MainThreadMarker::new().unwrap();
            app_state::handle_nonuser_event(mtm, gesture_event);
        }

        #[method(scrollGesture:)]
        fn scroll_gesture(&self, recognizer: &UIPanGestureRecognizer) {
            let window = self.window().unwrap();
            let translation = recognizer.translationInView(Some(self));
            let last = match recognizer.state() {
                UIGestureRecognizerState::Began => CGPoint { x: 0.0, y: 0.0 },
                _ => self.ivars().scroll_last.get(),
            };
            self.ivars().scroll_last.set(translation);
            let phase = match recognizer.state() {
                UIGestureRecognizerState::Began => TouchPhase::Started,
                UIGestureRecognizerState::Changed => TouchPhase::Moved,
                UIGestureRecognizerState::Ended => TouchPhase::Ended,
                _ => TouchPhase::Cancelled,
            };
            let scale = self.contentScaleFactor();
            let delta = PhysicalPosition::new(
                ((translation.x - last.x) * scale) as f64,
                ((translation.y - last.y) * scale) as f64,
            );
            app_state::handle_nonuser_event(
                MainThreadMarker::from(self),
                EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id: RootWindowId(window.id()),
                    event: WindowEvent::MouseWheel {
                        device_id: DEVICE_ID,
                        delta: MouseScrollDelta::PixelDelta(delta),
                        phase,
                    },
                }),
            );
        }

        #[method(canBecomeFirstResponder)]
        fn can_become_first_responder(&self) -> bool {
            true
        }
    }

    unsafe impl NSObjectProtocol for WinitView {}

    unsafe impl UIGestureRecognizerDelegate for WinitView {
        #[method(gestureRecognizer:shouldRecognizeSimultaneouslyWithGestureRecognizer:)]
        fn should_recognize_simultaneously(&self, _gesture_recognizer: &UIGestureRecognizer, _other_gesture_recognizer: &UIGestureRecognizer) -> bool {
            true
        }
    }

    unsafe impl UITextInputTraits for WinitView {
        #[method(keyboardType)]
        fn keyboard_type(&self) -> NSInteger {
            // UIKeyboardType
            match self.ivars().traits.get().keyboard {
                KeyboardType::NumberPad => 4,
                KeyboardType::DecimalPad => 8,
                KeyboardType::PhonePad => 5,
                KeyboardType::EmailAddress => 7,
                KeyboardType::Url => 3,
                _ => 0,
            }
        }

        #[method(returnKeyType)]
        fn return_key_type(&self) -> NSInteger {
            // UIReturnKeyType
            match self.ivars().traits.get().return_key {
                ReturnKeyType::Go => 1,
                ReturnKeyType::Next => 4,
                ReturnKeyType::Search => 6,
                ReturnKeyType::Send => 7,
                ReturnKeyType::Done => 9,
                _ => 0,
            }
        }

        #[method(isSecureTextEntry)]
        fn is_secure_text_entry(&self) -> bool {
            self.ivars().traits.get().secure
        }

        #[method(autocorrectionType)]
        fn autocorrection_type(&self) -> NSInteger {
            // UITextAutocorrectionType: No = 1, Yes = 2. Without a mirror
            // (the app never called `set_text_input_state`) a correction's
            // `replaceRange:` has nothing to edit and would vanish, so
            // offer none.
            let mirrored = self.ivars().text.borrow().is_some();
            if mirrored && self.ivars().traits.get().autocorrect { 2 } else { 1 }
        }

        #[method(autocapitalizationType)]
        fn autocapitalization_type(&self) -> NSInteger {
            // UITextAutocapitalizationType: None = 0, Sentences = 2
            if self.ivars().traits.get().autocapitalize { 2 } else { 0 }
        }
    }

    unsafe impl UIKeyInput for WinitView {
        #[method(hasText)]
        fn has_text(&self) -> bool {
            self.ivars().text.borrow().as_ref().map_or(true, |m| !m.text.is_empty())
        }

        #[method(insertText:)]
        fn insert_text(&self, text: &NSString) {
            let action = self.ivars().text.borrow().as_ref().map(|m| m.classify_insert(&text.to_string()));
            match action {
                None | Some(InsertAction::Enter) => self.handle_insert_text(text),
                Some(InsertAction::Edit) => self.edit(|m| m.insert(&text.to_string())),
            }
        }

        #[method(deleteBackward)]
        fn delete_backward(&self) {
            if self.ivars().text.borrow().is_some() {
                self.edit(TextMirror::delete_backward);
            } else {
                self.handle_delete_backward()
            }
        }
    }

    unsafe impl UITextInput for WinitView {
        #[method_id(textInRange:)]
        fn text_in_range(&self, range: &UITextRange) -> Option<Retained<NSString>> {
            (|| {
                let (a, b) = range_of(range)?;
                let text = self.ivars().text.borrow();
                let m = text.as_ref()?;
                Some(NSString::from_str(&m.text[m.byte_at(a)..m.byte_at(b)]))
            })()
        }

        #[method(replaceRange:withText:)]
        fn replace_range(&self, range: &UITextRange, text: &NSString) {
            if let Some(r) = range_of(range) {
                self.edit(|m| m.replace(r, &text.to_string()));
            }
        }

        #[method_id(selectedTextRange)]
        fn selected_text_range(&self) -> Option<Retained<UITextRange>> {
            (|| {
                let (a, b) = self.utf16(|m| m.selection)?;
                Some(WinitTextRange::between(MainThreadMarker::from(self), a, b))
            })()
        }

        #[method(setSelectedTextRange:)]
        fn set_selected_text_range(&self, range: Option<&UITextRange>) {
            if let Some(r) = range.and_then(range_of) {
                self.edit(|m| m.set_selection(r));
            }
        }

        #[method_id(markedTextRange)]
        fn marked_text_range(&self) -> Option<Retained<UITextRange>> {
            (|| {
                let text = self.ivars().text.borrow();
                let m = text.as_ref()?;
                let (a, b) = m.marked?;
                let (a, b) = (m.utf16_at(a), m.utf16_at(b));
                Some(WinitTextRange::between(MainThreadMarker::from(self), a, b))
            })()
        }

        #[method_id(markedTextStyle)]
        fn marked_text_style(&self) -> Option<Retained<AnyObject>> {
            None
        }

        #[method(setMarkedTextStyle:)]
        fn set_marked_text_style(&self, _style: Option<&AnyObject>) {}

        #[method(setMarkedText:selectedRange:)]
        fn set_marked_text(&self, text: Option<&NSString>, selected: NSRange) {
            let text = text.map(|t| t.to_string()).unwrap_or_default();
            let sel = (selected.location, selected.location + selected.length);
            self.edit(|m| m.set_marked(&text, sel));
        }

        #[method(unmarkText)]
        fn unmark_text(&self) {
            self.edit(TextMirror::unmark);
        }

        #[method_id(beginningOfDocument)]
        fn beginning_of_document(&self) -> Retained<UITextPosition> {
            WinitTextPosition::at(MainThreadMarker::from(self), 0)
        }

        #[method_id(endOfDocument)]
        fn end_of_document(&self) -> Retained<UITextPosition> {
            let end = self.ivars().text.borrow().as_ref().map_or(0, TextMirror::utf16_len);
            WinitTextPosition::at(MainThreadMarker::from(self), end)
        }

        #[method_id(textRangeFromPosition:toPosition:)]
        fn text_range_from_position(
            &self,
            from: &UITextPosition,
            to: &UITextPosition,
        ) -> Option<Retained<UITextRange>> {
            (|| {
                Some(WinitTextRange::between(MainThreadMarker::from(self), offset_of(from)?, offset_of(to)?))
            })()
        }

        #[method_id(positionFromPosition:offset:)]
        fn position_from_position(
            &self,
            position: &UITextPosition,
            offset: NSInteger,
        ) -> Option<Retained<UITextPosition>> {
            self.position_at(position, offset)
        }

        #[method_id(positionFromPosition:inDirection:offset:)]
        fn position_from_position_in_direction(
            &self,
            position: &UITextPosition,
            direction: NSInteger,
            offset: NSInteger,
        ) -> Option<Retained<UITextPosition>> {
            // UITextLayoutDirection: Right = 2, Left = 3, Up = 4, Down = 5.
            // One line: up and down stay put.
            let delta = match direction {
                2 => offset,
                3 => -offset,
                _ => 0,
            };
            self.position_at(position, delta)
        }

        #[method(comparePosition:toPosition:)]
        fn compare_position(&self, a: &UITextPosition, b: &UITextPosition) -> NSInteger {
            // NSComparisonResult
            match offset_of(a).cmp(&offset_of(b)) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            }
        }

        #[method(offsetFromPosition:toPosition:)]
        fn offset_from_position(&self, from: &UITextPosition, to: &UITextPosition) -> NSInteger {
            match (offset_of(from), offset_of(to)) {
                (Some(a), Some(b)) => b as NSInteger - a as NSInteger,
                _ => 0,
            }
        }

        #[method_id(inputDelegate)]
        fn input_delegate(&self) -> Option<Retained<AnyObject>> {
            self.ivars().input_delegate.borrow().clone()
        }

        #[method(setInputDelegate:)]
        fn set_input_delegate(&self, delegate: Option<&AnyObject>) {
            // SAFETY: a live object UIKit just handed us.
            *self.ivars().input_delegate.borrow_mut() = delegate.and_then(|d| unsafe {
                Retained::retain(d as *const AnyObject as *mut AnyObject)
            });
        }

        #[method_id(tokenizer)]
        fn tokenizer(&self) -> Retained<UITextInputStringTokenizer> {
            let mut slot = self.ivars().tokenizer.borrow_mut();
            slot.get_or_insert_with(|| unsafe {
                UITextInputStringTokenizer::initWithTextInput(
                    MainThreadMarker::from(self).alloc(),
                    self,
                )
            })
            .clone()
        }

        #[method_id(positionWithinRange:farthestInDirection:)]
        fn position_within_range(
            &self,
            range: &UITextRange,
            direction: NSInteger,
        ) -> Option<Retained<UITextPosition>> {
            (|| {
                let (a, b) = range_of(range)?;
                // Left (3) and up (4) mean the start.
                let at = if matches!(direction, 3 | 4) { a } else { b };
                Some(WinitTextPosition::at(MainThreadMarker::from(self), at))
            })()
        }

        #[method_id(characterRangeByExtendingPosition:inDirection:)]
        fn character_range_by_extending(
            &self,
            position: &UITextPosition,
            direction: NSInteger,
        ) -> Option<Retained<UITextRange>> {
            (|| {
                let at = offset_of(position)?;
                let len = self.ivars().text.borrow().as_ref().map_or(0, TextMirror::utf16_len);
                let (a, b) = if matches!(direction, 3 | 4) {
                    (at.saturating_sub(1), at)
                } else {
                    (at, (at + 1).min(len))
                };
                Some(WinitTextRange::between(MainThreadMarker::from(self), a, b))
            })()
        }

        #[method(baseWritingDirectionForPosition:inDirection:)]
        fn base_writing_direction(&self, _position: &UITextPosition, _direction: NSInteger) -> NSInteger {
            0 // NSWritingDirectionLeftToRight
        }

        #[method(setBaseWritingDirection:forRange:)]
        fn set_base_writing_direction(&self, _direction: NSInteger, _range: &UITextRange) {}

        #[method(firstRectForRange:)]
        fn first_rect_for_range(&self, _range: &UITextRange) -> CGRect {
            self.ivars().cursor_area.get()
        }

        #[method(caretRectForPosition:)]
        fn caret_rect_for_position(&self, _position: &UITextPosition) -> CGRect {
            let r = self.ivars().cursor_area.get();
            CGRect { origin: r.origin, size: CGSize { width: 2.0, height: r.size.height } }
        }

        #[method_id(selectionRectsForRange:)]
        fn selection_rects_for_range(&self, _range: &UITextRange) -> Retained<NSArray<NSObject>> {
            NSArray::new()
        }

        #[method_id(closestPositionToPoint:)]
        fn closest_position_to_point(&self, _point: CGPoint) -> Option<Retained<UITextPosition>> {
            (|| {
                let (_, b) = self.utf16(|m| m.selection)?;
                Some(WinitTextPosition::at(MainThreadMarker::from(self), b))
            })()
        }

        #[method_id(closestPositionToPoint:withinRange:)]
        fn closest_position_to_point_within(
            &self,
            _point: CGPoint,
            range: &UITextRange,
        ) -> Option<Retained<UITextPosition>> {
            (|| {
                let (_, b) = range_of(range)?;
                Some(WinitTextPosition::at(MainThreadMarker::from(self), b))
            })()
        }

        #[method_id(characterRangeAtPoint:)]
        fn character_range_at_point(&self, _point: CGPoint) -> Option<Retained<UITextRange>> {
            None
        }
    }
);

impl WinitView {
    pub(crate) fn new(
        mtm: MainThreadMarker,
        window_attributes: &WindowAttributes,
        frame: CGRect,
    ) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(WinitViewState {
            pinch_gesture_recognizer: RefCell::new(None),
            doubletap_gesture_recognizer: RefCell::new(None),
            rotation_gesture_recognizer: RefCell::new(None),
            pan_gesture_recognizer: RefCell::new(None),

            rotation_last_delta: Cell::new(0.0),
            pinch_last_delta: Cell::new(0.0),
            pan_last_delta: Cell::new(CGPoint { x: 0.0, y: 0.0 }),

            text: RefCell::new(None),
            traits: Cell::new(TextInputTraits::default()),
            input_delegate: RefCell::new(None),
            tokenizer: RefCell::new(None),
            cursor_area: Cell::new(CGRect::default()),

            keyboard_overlap: Cell::new(0.0),
            keyboard_observers: RefCell::new(Vec::new()),
            scroll_gesture_recognizer: RefCell::new(None),
            scroll_last: Cell::new(CGPoint { x: 0.0, y: 0.0 }),
        });
        let this: Retained<Self> = unsafe { msg_send_id![super(this), initWithFrame: frame] };

        this.setMultipleTouchEnabled(true);
        this.observe_keyboard();
        this.recognize_indirect_scroll();

        if let Some(scale_factor) = window_attributes.platform_specific.scale_factor {
            this.setContentScaleFactor(scale_factor as _);
        }

        this
    }

    fn window(&self) -> Option<Retained<WinitUIWindow>> {
        // SAFETY: `WinitView`s are always installed in a `WinitUIWindow`
        (**self).window().map(|window| unsafe { Retained::cast(window) })
    }

    pub(crate) fn recognize_pinch_gesture(&self, should_recognize: bool) {
        let mtm = MainThreadMarker::from(self);
        if should_recognize {
            if self.ivars().pinch_gesture_recognizer.borrow().is_none() {
                let pinch = unsafe {
                    UIPinchGestureRecognizer::initWithTarget_action(
                        mtm.alloc(),
                        Some(self),
                        Some(sel!(pinchGesture:)),
                    )
                };
                pinch.setDelegate(Some(ProtocolObject::from_ref(self)));
                self.addGestureRecognizer(&pinch);
                self.ivars().pinch_gesture_recognizer.replace(Some(pinch));
            }
        } else if let Some(recognizer) = self.ivars().pinch_gesture_recognizer.take() {
            self.removeGestureRecognizer(&recognizer);
        }
    }

    pub(crate) fn recognize_pan_gesture(
        &self,
        should_recognize: bool,
        minimum_number_of_touches: u8,
        maximum_number_of_touches: u8,
    ) {
        let mtm = MainThreadMarker::from(self);
        if should_recognize {
            if self.ivars().pan_gesture_recognizer.borrow().is_none() {
                let pan = unsafe {
                    UIPanGestureRecognizer::initWithTarget_action(
                        mtm.alloc(),
                        Some(self),
                        Some(sel!(panGesture:)),
                    )
                };
                pan.setDelegate(Some(ProtocolObject::from_ref(self)));
                pan.setMinimumNumberOfTouches(minimum_number_of_touches as _);
                pan.setMaximumNumberOfTouches(maximum_number_of_touches as _);
                self.addGestureRecognizer(&pan);
                self.ivars().pan_gesture_recognizer.replace(Some(pan));
            }
        } else if let Some(recognizer) = self.ivars().pan_gesture_recognizer.take() {
            self.removeGestureRecognizer(&recognizer);
        }
    }

    pub(crate) fn recognize_doubletap_gesture(&self, should_recognize: bool) {
        let mtm = MainThreadMarker::from(self);
        if should_recognize {
            if self.ivars().doubletap_gesture_recognizer.borrow().is_none() {
                let tap = unsafe {
                    UITapGestureRecognizer::initWithTarget_action(
                        mtm.alloc(),
                        Some(self),
                        Some(sel!(doubleTapGesture:)),
                    )
                };
                tap.setDelegate(Some(ProtocolObject::from_ref(self)));
                tap.setNumberOfTapsRequired(2);
                tap.setNumberOfTouchesRequired(1);
                self.addGestureRecognizer(&tap);
                self.ivars().doubletap_gesture_recognizer.replace(Some(tap));
            }
        } else if let Some(recognizer) = self.ivars().doubletap_gesture_recognizer.take() {
            self.removeGestureRecognizer(&recognizer);
        }
    }

    pub(crate) fn recognize_rotation_gesture(&self, should_recognize: bool) {
        let mtm = MainThreadMarker::from(self);
        if should_recognize {
            if self.ivars().rotation_gesture_recognizer.borrow().is_none() {
                let rotation = unsafe {
                    UIRotationGestureRecognizer::initWithTarget_action(
                        mtm.alloc(),
                        Some(self),
                        Some(sel!(rotationGesture:)),
                    )
                };
                rotation.setDelegate(Some(ProtocolObject::from_ref(self)));
                self.addGestureRecognizer(&rotation);
                self.ivars().rotation_gesture_recognizer.replace(Some(rotation));
            }
        } else if let Some(recognizer) = self.ivars().rotation_gesture_recognizer.take() {
            self.removeGestureRecognizer(&recognizer);
        }
    }

    fn handle_touches(&self, touches: &NSSet<UITouch>) {
        let window = self.window().unwrap();
        let mut touch_events = Vec::new();
        let os_supports_force = app_state::os_capabilities().force_touch;
        for touch in touches {
            let logical_location = touch.locationInView(None);
            let touch_type = touch.r#type();
            let force = if os_supports_force {
                let trait_collection = self.traitCollection();
                let touch_capability = trait_collection.forceTouchCapability();
                // Both the OS _and_ the device need to be checked for force touch support.
                if touch_capability == UIForceTouchCapability::Available
                    || touch_type == UITouchType::Pencil
                {
                    let force = touch.force();
                    let max_possible_force = touch.maximumPossibleForce();
                    let altitude_angle: Option<f64> = if touch_type == UITouchType::Pencil {
                        let angle = touch.altitudeAngle();
                        Some(angle as _)
                    } else {
                        None
                    };
                    Some(Force::Calibrated {
                        force: force as _,
                        max_possible_force: max_possible_force as _,
                        altitude_angle,
                    })
                } else {
                    None
                }
            } else {
                None
            };
            let touch_id = touch as *const UITouch as u64;
            let phase = touch.phase();
            let phase = match phase {
                UITouchPhase::Began => TouchPhase::Started,
                UITouchPhase::Moved => TouchPhase::Moved,
                // 2 is UITouchPhase::Stationary and is not expected here
                UITouchPhase::Ended => TouchPhase::Ended,
                UITouchPhase::Cancelled => TouchPhase::Cancelled,
                _ => panic!("unexpected touch phase: {phase:?}"),
            };

            let physical_location = {
                let scale_factor = self.contentScaleFactor();
                PhysicalPosition::from_logical::<(f64, f64), f64>(
                    (logical_location.x as _, logical_location.y as _),
                    scale_factor as f64,
                )
            };
            touch_events.push(EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::Touch(Touch {
                    device_id: DEVICE_ID,
                    id: touch_id,
                    location: physical_location,
                    force,
                    phase,
                }),
            }));
        }
        let mtm = MainThreadMarker::new().unwrap();
        app_state::handle_nonuser_events(mtm, touch_events);
    }

    /// Tracks the soft keyboard: its overlap with the window joins the safe
    /// area, and a relayout (`layoutSubviews`, which sends `Resized`) tells
    /// the app to read it again.
    fn observe_keyboard(&self) {
        let center = unsafe { objc2_foundation::NSNotificationCenter::defaultCenter() };
        let weak = objc2::rc::Weak::from_retained(&self.retain());
        let changed = {
            let weak = weak.clone();
            move |notification: &objc2_foundation::NSNotification| {
                let Some(view) = weak.load() else { return };
                let frame: Option<CGRect> = unsafe {
                    notification.userInfo().and_then(|info| {
                        let key: &AnyObject = objc2_ui_kit::UIKeyboardFrameEndUserInfoKey.as_ref();
                        let value: Option<Retained<AnyObject>> =
                            msg_send_id![&*info, objectForKey: key];
                        value.map(|v| msg_send![&*v, CGRectValue])
                    })
                };
                view.set_keyboard_frame(frame);
            }
        };
        let hidden = move |_: &objc2_foundation::NSNotification| {
            if let Some(view) = weak.load() {
                view.set_keyboard_frame(None);
            }
        };
        let observers = unsafe {
            vec![
                super::notification_center::create_observer(
                    &center,
                    objc2_ui_kit::UIKeyboardWillChangeFrameNotification,
                    changed,
                ),
                super::notification_center::create_observer(
                    &center,
                    objc2_ui_kit::UIKeyboardWillHideNotification,
                    hidden,
                ),
            ]
        };
        *self.ivars().keyboard_observers.borrow_mut() = observers;
    }

    /// The keyboard's end frame in screen coordinates (`None`: hidden).
    fn set_keyboard_frame(&self, frame: Option<CGRect>) {
        let Some(window) = self.window() else { return };
        let overlap = frame.map_or(0.0, |k| {
            let screen_space = window.screen().coordinateSpace();
            let w = self.convertRect_toCoordinateSpace(window.bounds(), &screen_space);
            crate::platform_impl::ios_keyboard::keyboard_overlap(
                (w.origin.x as f64, w.origin.y as f64, w.size.width as f64, w.size.height as f64),
                (k.origin.x as f64, k.origin.y as f64, k.size.width as f64, k.size.height as f64),
            ) as CGFloat
        });
        if self.ivars().keyboard_overlap.replace(overlap) != overlap {
            unsafe { self.setNeedsLayout() };
        }
    }

    /// How much of the window the keyboard covers, in points.
    pub(crate) fn keyboard_overlap(&self) -> CGFloat {
        self.ivars().keyboard_overlap.get()
    }

    /// Trackpad and mouse-wheel scrolling as `MouseWheel`: a pan recognizer
    /// that takes indirect scroll input and no touches.
    fn recognize_indirect_scroll(&self) {
        let mtm = MainThreadMarker::from(self);
        let pan = unsafe {
            UIPanGestureRecognizer::initWithTarget_action(
                mtm.alloc(),
                Some(self),
                Some(sel!(scrollGesture:)),
            )
        };
        unsafe {
            pan.setAllowedScrollTypesMask(objc2_ui_kit::UIScrollTypeMask::All);
            // Trackpad and wheel scrolls arrive as indirect-pointer input;
            // an empty list filtered them out too. Fingers stay with the
            // view's own touch handling.
            let indirect_pointer =
                objc2_foundation::NSNumber::new_isize(UITouchType::IndirectPointer.0);
            pan.setAllowedTouchTypes(&NSArray::from_vec(vec![indirect_pointer]));
        }
        pan.setDelegate(Some(ProtocolObject::from_ref(self)));
        self.addGestureRecognizer(&pan);
        self.ivars().scroll_gesture_recognizer.replace(Some(pan));
    }

    /// The position `offset` UTF-16 units from `position`, if inside the text.
    fn position_at(
        &self,
        position: &UITextPosition,
        offset: NSInteger,
    ) -> Option<Retained<UITextPosition>> {
        let at = offset_of(position)? as NSInteger + offset;
        let len = self.ivars().text.borrow().as_ref().map_or(0, TextMirror::utf16_len);
        (0..=len as NSInteger)
            .contains(&at)
            .then(|| WinitTextPosition::at(MainThreadMarker::from(self), at as usize))
    }

    /// `f`'s byte range of the mirror, in UTF-16 offsets.
    fn utf16(&self, f: impl Fn(&TextMirror) -> (usize, usize)) -> Option<(usize, usize)> {
        let text = self.ivars().text.borrow();
        let m = text.as_ref()?;
        let (a, b) = f(m);
        Some((m.utf16_at(a), m.utf16_at(b)))
    }

    /// Applies an input method's edit to the mirror and, when it changed
    /// anything, reports the new state as `Ime::TextState`.
    fn edit(&self, f: impl FnOnce(&mut TextMirror) -> bool) {
        let state = {
            let mut text = self.ivars().text.borrow_mut();
            let Some(m) = text.as_mut() else { return };
            if !f(m) {
                return;
            }
            Ime::TextState { text: m.text.clone(), selection: m.selection, compose: m.marked }
        };
        let Some(window) = self.window() else { return };
        app_state::handle_nonuser_event(
            MainThreadMarker::from(self),
            EventWrapper::StaticEvent(Event::WindowEvent {
                window_id: RootWindowId(window.id()),
                event: WindowEvent::Ime(state),
            }),
        );
    }

    /// The app's field (`WindowExtIOS::set_text_input_state`). Tells the
    /// input method the text changed under it, so it drops what it was
    /// composing against the old text.
    pub(crate) fn set_text_input_state(&self, state: Option<TextInputState>) {
        let mirror = state.map(|s| TextMirror::from_state(s.text, s.selection, s.compose));
        if *self.ivars().text.borrow() == mirror {
            return;
        }
        let delegate = self.ivars().input_delegate.borrow().clone();
        let me: &AnyObject = self.as_ref();
        if let Some(d) = &delegate {
            unsafe {
                let _: () = msg_send![d, selectionWillChange: me];
                let _: () = msg_send![d, textWillChange: me];
            }
        }
        *self.ivars().text.borrow_mut() = mirror;
        if let Some(d) = &delegate {
            unsafe {
                let _: () = msg_send![d, textDidChange: me];
                let _: () = msg_send![d, selectionDidChange: me];
            }
        }
    }

    /// The keyboard's traits; reloads the keyboard when it is up.
    pub(crate) fn set_text_input_traits(&self, traits: TextInputTraits) {
        if self.ivars().traits.replace(traits) != traits && unsafe { self.isFirstResponder() } {
            unsafe { self.reloadInputViews() };
        }
    }

    /// The caret's rect in view points, for the candidate bar.
    pub(crate) fn set_cursor_area(&self, rect: CGRect) {
        self.ivars().cursor_area.set(rect);
    }

    fn handle_insert_text(&self, text: &NSString) {
        let window = self.window().unwrap();
        let window_id = RootWindowId(window.id());
        let mtm = MainThreadMarker::new().unwrap();
        // send individual events for each character
        app_state::handle_nonuser_events(
            mtm,
            text.to_string().chars().flat_map(|c| {
                let text = smol_str::SmolStr::from_iter([c]);
                // Emit both press and release events
                [ElementState::Pressed, ElementState::Released].map(|state| {
                    EventWrapper::StaticEvent(Event::WindowEvent {
                        window_id,
                        event: WindowEvent::KeyboardInput {
                            event: KeyEvent {
                                text: if state == ElementState::Pressed {
                                    Some(text.clone())
                                } else {
                                    None
                                },
                                state,
                                location: KeyLocation::Standard,
                                repeat: false,
                                logical_key: Key::Character(text.clone()),
                                physical_key: PhysicalKey::Unidentified(
                                    NativeKeyCode::Unidentified,
                                ),
                                platform_specific: KeyEventExtra {},
                            },
                            is_synthetic: false,
                            device_id: DEVICE_ID,
                        },
                    })
                })
            }),
        );
    }

    fn handle_delete_backward(&self) {
        let window = self.window().unwrap();
        let window_id = RootWindowId(window.id());
        let mtm = MainThreadMarker::new().unwrap();
        app_state::handle_nonuser_events(
            mtm,
            [ElementState::Pressed, ElementState::Released].map(|state| {
                EventWrapper::StaticEvent(Event::WindowEvent {
                    window_id,
                    event: WindowEvent::KeyboardInput {
                        device_id: DEVICE_ID,
                        event: KeyEvent {
                            state,
                            logical_key: Key::Named(NamedKey::Backspace),
                            physical_key: PhysicalKey::Code(KeyCode::Backspace),
                            platform_specific: KeyEventExtra {},
                            repeat: false,
                            location: KeyLocation::Standard,
                            text: None,
                        },
                        is_synthetic: false,
                    },
                })
            }),
        );
    }
}
