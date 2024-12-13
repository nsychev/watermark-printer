use lopdf::content::Operation;
use lopdf::Document;
use lopdf::Object::Name;
use crate::drawer::Image;
use crate::image_xobject::ImageXObject;

pub fn apply_watermark(document: &mut Document, image: Image, left: f32, top: f32) -> anyhow::Result<()> {
    let (mut image_xobject, mask_xobject) = ImageXObject::try_from(image.width, image.height, image.data)?;

    let mask_id = document.add_object(mask_xobject);
    image_xobject.s_mask = Some(mask_id);

    let image_id = document.add_object(image_xobject);

    for (_page_number, page_id) in document.get_pages() {
        let xobject_id = format!("w{}-{}", page_id.0, page_id.1);
        document.add_xobject(page_id, xobject_id.as_bytes(), image_id)?;

        let mut content = document.get_and_decode_page_content(page_id)?;

        content.operations.push(Operation::new("q", vec![]));
        content.operations.push(Operation::new(
            "cm",
            vec![
                (image.width as f32).into(),
                0i32.into(),
                0i32.into(),
                (image.height as f32).into(),
                left.into(),
                top.into()
            ]
        ));
        content.operations.push(Operation::new(
            "Do",
            vec![Name(xobject_id.as_bytes().to_vec())]
        ));

        content.operations.push(Operation::new("Q", vec![]));

        document.change_page_content(page_id, content.encode()?)?;
    }

    document.compress();

    Ok(())
}
