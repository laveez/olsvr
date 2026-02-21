use rand::Rng;
use std::time::{Duration, Instant};

#[derive(Debug)]
enum Phase {
    FadeIn,
    Hold,
    FadeOut,
    Teleport,
}

pub struct Animation {
    phase: Phase,
    phase_start: Instant,
    fade_duration: Duration,
    hold_duration: Duration,
    x: f32,
    y: f32,
    last_quadrant: u8,
    screen_width: f32,
    screen_height: f32,
    text_width: f32,
    text_height: f32,
}

impl Animation {
    pub fn new(
        screen_width: f32,
        screen_height: f32,
        text_width: f32,
        text_height: f32,
        hold_secs: u32,
    ) -> Self {
        let mut anim = Self {
            phase: Phase::Teleport,
            phase_start: Instant::now(),
            fade_duration: Duration::from_millis(1500),
            hold_duration: Duration::from_secs(hold_secs as u64),
            x: 0.0,
            y: 0.0,
            last_quadrant: u8::MAX, // so first pick is unconstrained
            screen_width,
            screen_height,
            text_width,
            text_height,
        };
        // Immediately pick first position
        anim.random_position();
        anim.phase = Phase::FadeIn;
        anim.phase_start = Instant::now();
        anim
    }

    pub fn alpha(&self) -> u8 {
        let elapsed = self.phase_start.elapsed();
        match self.phase {
            Phase::FadeIn => {
                let t = elapsed.as_secs_f32() / self.fade_duration.as_secs_f32();
                (t.clamp(0.0, 1.0) * 255.0) as u8
            }
            Phase::Hold => 255,
            Phase::FadeOut => {
                let t = elapsed.as_secs_f32() / self.fade_duration.as_secs_f32();
                ((1.0 - t.clamp(0.0, 1.0)) * 255.0) as u8
            }
            Phase::Teleport => 0,
        }
    }

    pub fn position(&self) -> (f32, f32) {
        (self.x, self.y)
    }

    /// Returns true if a new frame is needed (during fade phases).
    pub fn needs_frame(&self) -> bool {
        matches!(self.phase, Phase::FadeIn | Phase::FadeOut)
    }

    pub fn tick(&mut self) {
        let elapsed = self.phase_start.elapsed();
        match self.phase {
            Phase::FadeIn if elapsed >= self.fade_duration => {
                self.phase = Phase::Hold;
                self.phase_start = Instant::now();
            }
            Phase::Hold if elapsed >= self.hold_duration => {
                self.phase = Phase::FadeOut;
                self.phase_start = Instant::now();
            }
            Phase::FadeOut if elapsed >= self.fade_duration => {
                self.random_position();
                self.phase = Phase::FadeIn;
                self.phase_start = Instant::now();
            }
            Phase::Teleport => {
                self.random_position();
                self.phase = Phase::FadeIn;
                self.phase_start = Instant::now();
            }
            _ => {}
        }
    }

    pub fn update_screen_size(
        &mut self,
        screen_width: f32,
        screen_height: f32,
        text_width: f32,
        text_height: f32,
    ) {
        self.screen_width = screen_width;
        self.screen_height = screen_height;
        self.text_width = text_width;
        self.text_height = text_height;
    }

    fn random_position(&mut self) {
        let mut rng = rand::rng();

        let quadrant = if self.last_quadrant == u8::MAX {
            rng.random_range(0..4u8)
        } else {
            loop {
                let q = rng.random_range(0..4u8);
                if q != self.last_quadrant {
                    break q;
                }
            }
        };
        self.last_quadrant = quadrant;

        let half_w = self.screen_width / 2.0;
        let half_h = self.screen_height / 2.0;
        let pad = 50.0;

        let (x_min, x_max) = match quadrant % 2 {
            0 => (pad, (half_w - self.text_width - pad).max(pad + 1.0)),
            _ => (
                half_w + pad,
                (self.screen_width - self.text_width - pad).max(half_w + pad + 1.0),
            ),
        };
        let (y_min, y_max) = match quadrant / 2 {
            0 => (pad, (half_h - self.text_height - pad).max(pad + 1.0)),
            _ => (
                half_h + pad,
                (self.screen_height - self.text_height - pad).max(half_h + pad + 1.0),
            ),
        };

        self.x = rng.random_range(x_min..x_max);
        self.y = rng.random_range(y_min..y_max);
    }
}
