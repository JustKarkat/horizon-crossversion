// A click on the version switch is ours; anything else goes to the screen.
fn click_switch(
    screen: &mut Screen,
    x: f32,
    y: f32,
    size: (f32, f32),
) -> std::ops::ControlFlow<Outcome> {
    if crate::multiplayer::crossversion_switch::click(screen, x, y, size) {
        std::ops::ControlFlow::Break(Outcome::None)
    } else {
        std::ops::ControlFlow::Continue(())
    }
}
