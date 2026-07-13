use bootycall_oled::framebuffer::Framebuffer;
use bootycall_oled::renderer::{FONTS, Renderer};

#[test]
fn test_degraded_no_text_mode_no_panic_and_returns_zero() {
    // 1. Initialize the OnceLock to None to simulate font load failure
    assert!(
        FONTS.set(None).is_ok(),
        "OnceLock FONTS should be empty at test start"
    );

    // 2. Setup Framebuffer
    let mut fb = Framebuffer::new();

    // 3. Call draw_text and verify it does not panic and doesn't change the framebuffer
    {
        let mut renderer = Renderer::new(&mut fb);
        renderer.draw_text(0, 0, "Hello World", false);
        renderer.draw_text(0, 0, "Hello World", true);
    }

    // Check that framebuffer buffer is all zeros (nothing drawn)
    assert!(
        fb.buffer.iter().all(|&pixel| pixel == 0),
        "degraded mode should not draw any text"
    );

    // 4. Call draw_text_scaled and verify it does not panic and doesn't change the framebuffer
    {
        let mut renderer = Renderer::new(&mut fb);
        renderer.draw_text_scaled(0, 0, "Hello World", false, 12.0);
    }
    assert!(
        fb.buffer.iter().all(|&pixel| pixel == 0),
        "degraded mode scaled should not draw any text"
    );

    // 5. Call measure_text and verify it returns 0 without panicking
    let width_small = Renderer::measure_text("Hello World", false);
    let width_large = Renderer::measure_text("Hello World", true);
    assert_eq!(
        width_small, 0,
        "degraded mode small text measurement should be 0"
    );
    assert_eq!(
        width_large, 0,
        "degraded mode large text measurement should be 0"
    );

    // 6. Call measure_text_scaled and verify it returns 0 without panicking
    let width_scaled = Renderer::measure_text_scaled("Hello World", false, 12.0);
    assert_eq!(
        width_scaled, 0,
        "degraded mode scaled text measurement should be 0"
    );
}
