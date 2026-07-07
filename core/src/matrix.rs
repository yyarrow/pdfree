/// 2D affine transform stored as [a b c d e f], mapping
/// (x, y) -> (a*x + c*y + e, b*x + d*y + f).
#[derive(Clone, Copy, Debug)]
pub struct Mat(pub [f32; 6]);

impl Mat {
    pub fn identity() -> Self {
        Mat([1.0, 0.0, 0.0, 1.0, 0.0, 0.0])
    }

    pub fn translate(tx: f32, ty: f32) -> Self {
        Mat([1.0, 0.0, 0.0, 1.0, tx, ty])
    }

    /// self * other (apply self first, then other), matching PDF's
    /// convention where Tm is multiplied into CTM.
    pub fn mul(&self, other: &Mat) -> Mat {
        let a = self.0;
        let b = other.0;
        Mat([
            a[0] * b[0] + a[1] * b[2],
            a[0] * b[1] + a[1] * b[3],
            a[2] * b[0] + a[3] * b[2],
            a[2] * b[1] + a[3] * b[3],
            a[4] * b[0] + a[5] * b[2] + b[4],
            a[4] * b[1] + a[5] * b[3] + b[5],
        ])
    }

    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        let m = self.0;
        (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
    }
}
