// SPDX-License-Identifier: GPL-3.0-or-later
//! [`Pinned`]: an element drawn at the physical rectangle it was built for, at any scale.
//!
//! Its own module, with nothing of the compositor's in it, so `examples/test_hdr_encode.rs` can
//! include it beside `hdr_render.rs` by path.

use smithay::backend::renderer::Renderer;
use smithay::backend::renderer::element::{Element, Kind, RenderElement};
use smithay::utils::{Physical, Point, Rectangle, Scale};

/// An element drawn at exactly the physical rectangle it was built for, whatever scale it is
/// rendered at.
///
/// The compositor's own chrome is laid out in physical pixels: every quad goes through
/// `viewport.rect(..)` into `SolidColorRenderElement`, whose geometry is physical and ignores the
/// scale. A memory buffer is different -- smithay treats its size as *logical* and multiplies it
/// by the scale the damage tracker renders at -- so text and icon pictures laid out the same way
/// came out the right size only where that scale was 1. The screen is rendered at the output's
/// scale, so on a 150 % laptop every title was half again too big and ran off the bottom of its
/// titlebar; screenshots were rendered at 1, which is why they looked right and the screen did
/// not.
///
/// Rather than dividing the size by the scale -- which rounds to whole logical pixels and then
/// stretches the text by one at 125 % and 150 %, blurring it -- the inner element is asked about
/// itself at scale 1, where its "logical" size is the physical size it was given.
#[derive(Debug)]
pub struct Pinned<E>(pub E);

const UNSCALED: Scale<f64> = Scale { x: 1.0, y: 1.0 };

impl<E: Element> Element for Pinned<E> {
    fn id(&self) -> &smithay::backend::renderer::element::Id {
        self.0.id()
    }

    fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
        self.0.current_commit()
    }

    fn location(&self, _scale: Scale<f64>) -> Point<i32, Physical> {
        self.0.location(UNSCALED)
    }

    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        self.0.src()
    }

    fn transform(&self) -> smithay::utils::Transform {
        self.0.transform()
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.0.geometry(UNSCALED)
    }

    fn damage_since(
        &self,
        _scale: Scale<f64>,
        commit: Option<smithay::backend::renderer::utils::CommitCounter>,
    ) -> smithay::backend::renderer::utils::DamageSet<i32, Physical> {
        self.0.damage_since(UNSCALED, commit)
    }

    fn opaque_regions(
        &self,
        _scale: Scale<f64>,
    ) -> smithay::backend::renderer::utils::OpaqueRegions<i32, Physical> {
        self.0.opaque_regions(UNSCALED)
    }

    fn alpha(&self) -> f32 {
        self.0.alpha()
    }

    fn kind(&self) -> Kind {
        self.0.kind()
    }
}

impl<R, E> RenderElement<R> for Pinned<E>
where
    R: Renderer,
    E: RenderElement<R>,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, smithay::utils::Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), R::Error> {
        self.0.draw(frame, src, dst, damage, opaque_regions, cache)
    }

    fn underlying_storage(
        &self,
        renderer: &mut R,
    ) -> Option<smithay::backend::renderer::element::UnderlyingStorage<'_>> {
        self.0.underlying_storage(renderer)
    }
}
