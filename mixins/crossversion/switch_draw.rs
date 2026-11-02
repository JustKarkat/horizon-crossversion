// After the screen is drawn: the version switch on Edit Server and Direct Connect.
fn draw_switch(screen: &Screen, atlas: &HudAtlas, batch: &mut HudBatch) {
    crate::multiplayer::crossversion_switch::draw(
        screen,
        atlas,
        batch,
        (batch.width, batch.height),
    );
}
