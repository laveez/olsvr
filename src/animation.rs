use rand::{Rng, RngExt};
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
    edge_padding: f32,
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
        fade_duration_ms: u32,
        edge_padding: u32,
    ) -> Self {
        let mut anim = Self {
            phase: Phase::Teleport,
            phase_start: Instant::now(),
            fade_duration: Duration::from_millis(fade_duration_ms as u64),
            hold_duration: Duration::from_secs(hold_secs as u64),
            edge_padding: edge_padding as f32,
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

    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::FadeIn => "FadeIn",
            Phase::Hold => "Hold",
            Phase::FadeOut => "FadeOut",
            Phase::Teleport => "Teleport",
        }
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

        let pad = self.edge_padding;
        self.x = random_start(
            &mut rng,
            self.screen_width,
            self.text_width,
            pad,
            quadrant % 2 == 1,
        );
        self.y = random_start(
            &mut rng,
            self.screen_height,
            self.text_height,
            pad,
            quadrant / 2 == 1,
        );
    }
}

/// Where a block of `size` starts along an axis of `screen` pixels: anywhere in the chosen half
/// that keeps `pad` clear of the screen edges and the centre line. A block too big for its half
/// may start anywhere that keeps `pad` clear of both edges, and one too big for that is centred.
fn random_start(rng: &mut impl Rng, screen: f32, size: f32, pad: f32, second_half: bool) -> f32 {
    let half = screen / 2.0;
    let in_half = if second_half {
        (half + pad, screen - size - pad)
    } else {
        (pad, half - size - pad)
    };
    let on_screen = (pad, screen - size - pad);
    let (min, max) = if in_half.0 < in_half.1 {
        in_half
    } else {
        on_screen
    };
    if min < max {
        rng.random_range(min..max)
    } else {
        (screen - size) / 2.0
    }
}
