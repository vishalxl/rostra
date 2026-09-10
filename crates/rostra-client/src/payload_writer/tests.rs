use std::cell::Cell;
use std::io::Write as _;
use std::rc::Rc;

use rostra_client_db::PayloadAdmissionPause;
use rostra_core::event::content_kind::{EventContentKind as _, SocialPost};

use super::{GrowCapacity, PayloadWriter};

struct Capacity {
    limit: u64,
    charged: Rc<Cell<u64>>,
}

impl GrowCapacity for Capacity {
    fn try_grow(&mut self, bytes: u64) -> Result<(), PayloadAdmissionPause> {
        if bytes > self.limit {
            return Err(PayloadAdmissionPause::InFlightBytes);
        }
        self.charged.set(self.charged.get().max(bytes));
        Ok(())
    }
}

impl Drop for Capacity {
    fn drop(&mut self) {
        self.charged.set(0);
    }
}

#[test]
fn output_growth_is_charged_before_write_and_refusal_preserves_bytes() {
    let charged = Rc::new(Cell::new(0));
    let mut writer = PayloadWriter::new(Capacity {
        limit: 200,
        charged: charged.clone(),
    });
    writer.write_all(&[1; 100]).unwrap();
    assert_eq!(charged.get(), 128);
    writer.write_all(&[2; 100]).unwrap();
    assert_eq!(
        charged.get(),
        200,
        "geometric growth falls back to exact budget"
    );
    assert!(writer.write_all(&[3]).is_err());
    assert_eq!(writer.bytes.len(), 200);
    assert_eq!(charged.get(), 200);
    assert_eq!(writer.pause, Some(PayloadAdmissionPause::InFlightBytes));
    drop(writer);
    assert_eq!(charged.get(), 0);
}

#[test]
fn cbor_matches_ordinary_encoding_and_budget_refusal_is_typed() {
    let content = SocialPost::new_text("serialization".repeat(100), None, Default::default());
    let expected = content.serialize_cbor().unwrap();
    let charged = Rc::new(Cell::new(0));
    let mut writer = PayloadWriter::new(Capacity {
        limit: expected.len() as u64,
        charged,
    });
    content.serialize_cbor_to_writer(&mut writer).unwrap();
    assert_eq!(writer.bytes.as_slice(), expected.as_ref());
    let charged = Rc::new(Cell::new(0));
    let mut short = PayloadWriter::new(Capacity {
        limit: 10,
        charged: charged.clone(),
    });
    assert!(content.serialize_cbor_to_writer(&mut short).is_err());
    assert_eq!(short.pause, Some(PayloadAdmissionPause::InFlightBytes));
    assert!(short.bytes.len() <= 10);
    assert!(charged.get() <= 10);
}
