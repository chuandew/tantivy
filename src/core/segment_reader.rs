use Result;
use core::Segment;
use core::SegmentId;
use core::SegmentComponent;
use std::sync::RwLock;
use common::HasLen;
use core::SegmentMeta;
use fastfield::{self, FastFieldNotAvailableError};
use fastfield::DeleteBitSet;
use store::StoreReader;
use schema::Document;
use DocId;
use std::str;
use std::sync::Arc;
use std::collections::HashMap;
use common::CompositeFile;
use std::fmt;
use core::FieldReader;
use schema::Field;
use fastfield::{FastFieldsReader, FastFieldReader, U64FastFieldReader};
use schema::Schema;



/// Entry point to access all of the datastructures of the `Segment`
///
/// - term dictionary
/// - postings
/// - store
/// - fast field readers
/// - field norm reader
///
/// The segment reader has a very low memory footprint,
/// as close to all of the memory data is mmapped.
///
#[derive(Clone)]
pub struct SegmentReader {
    field_reader_cache: Arc<RwLock<HashMap<Field, Arc<FieldReader>>>>,

    segment_id: SegmentId,
    segment_meta: SegmentMeta,

    termdict_composite: CompositeFile,
    postings_composite: CompositeFile,
    positions_composite: CompositeFile,

    store_reader: StoreReader,
    fast_fields_reader: Arc<FastFieldsReader>,
    fieldnorms_reader: Arc<FastFieldsReader>,
    delete_bitset: DeleteBitSet,
    schema: Schema,
}

impl SegmentReader {
    /// Returns the highest document id ever attributed in
    /// this segment + 1.
    /// Today, `tantivy` does not handle deletes, so it happens
    /// to also be the number of documents in the index.
    pub fn max_doc(&self) -> DocId {
        self.segment_meta.max_doc()
    }

    /// Returns the number of documents.
    /// Deleted documents are not counted.
    ///
    /// Today, `tantivy` does not handle deletes so max doc and
    /// num_docs are the same.
    pub fn num_docs(&self) -> DocId {
        self.segment_meta.num_docs()
    }

    /// Return the number of documents that have been
    /// deleted in the segment.
    pub fn num_deleted_docs(&self) -> DocId {
        self.delete_bitset.len() as DocId
    }

    #[doc(hidden)]
    pub fn fast_fields_reader(&self) -> &FastFieldsReader {
        &*self.fast_fields_reader
    }

    /// Accessor to a segment's fast field reader given a field.
    ///
    /// Returns the u64 fast value reader if the field
    /// is a u64 field indexed as "fast".
    ///
    /// Return a FastFieldNotAvailableError if the field is not
    /// declared as a fast field in the schema.
    ///
    /// # Panics
    /// May panic if the index is corrupted.
    pub fn get_fast_field_reader<TFastFieldReader: FastFieldReader>
        (&self,
         field: Field)
         -> fastfield::Result<TFastFieldReader> {
        let field_entry = self.schema.get_field_entry(field);
        if !TFastFieldReader::is_enabled(field_entry.field_type()) {
            Err(FastFieldNotAvailableError::new(field_entry))
        } else {
            Ok(self.fast_fields_reader
                   .open_reader(field)
                   .expect("Fast field file corrupted."))
        }
    }

    /// Accessor to the segment's `Field norms`'s reader.
    ///
    /// Field norms are the length (in tokens) of the fields.
    /// It is used in the computation of the [TfIdf]
    /// (https://fulmicoton.gitbooks.io/tantivy-doc/content/tfidf.html).
    ///
    /// They are simply stored as a fast field, serialized in
    /// the `.fieldnorm` file of the segment.
    pub fn get_fieldnorms_reader(&self, field: Field) -> Option<U64FastFieldReader> {
        self.fieldnorms_reader.open_reader(field)
    }

    /// Accessor to the segment's `StoreReader`.
    pub fn get_store_reader(&self) -> &StoreReader {
        &self.store_reader
    }

    /// Open a new segment for reading.
    pub fn open(segment: Segment) -> Result<SegmentReader> {

        let termdict_source = segment.open_read(SegmentComponent::TERMS)?;
        let termdict_composite = CompositeFile::open(termdict_source)?;

        let store_source = segment.open_read(SegmentComponent::STORE)?;
        let store_reader = StoreReader::from_source(store_source);

        let postings_source = segment.open_read(SegmentComponent::POSTINGS)?;
        let postings_composite =  CompositeFile::open(postings_source)?;

        let positions_composite = {
            if let Ok(source) = segment.open_read(SegmentComponent::POSITIONS) {
                CompositeFile::open(source)?
            }
            else {
                CompositeFile::empty()
            }
        };


        let fast_field_data = segment.open_read(SegmentComponent::FASTFIELDS)?;
        let fast_fields_reader = FastFieldsReader::from_source(fast_field_data)?;

        let fieldnorms_data = segment.open_read(SegmentComponent::FIELDNORMS)?;
        let fieldnorms_reader = FastFieldsReader::from_source(fieldnorms_data)?;


        let delete_bitset = if segment.meta().has_deletes() {
            let delete_data = segment.open_read(SegmentComponent::DELETE)?;
            DeleteBitSet::open(delete_data)
        } else {
            DeleteBitSet::empty()
        };

        let schema = segment.schema();
        Ok(SegmentReader {
           field_reader_cache: Arc::new(RwLock::new(HashMap::new())),
           segment_meta: segment.meta().clone(),
           postings_composite: postings_composite,
           termdict_composite: termdict_composite,
           segment_id: segment.id(),
           store_reader: store_reader,
           fast_fields_reader: Arc::new(fast_fields_reader),
           fieldnorms_reader: Arc::new(fieldnorms_reader),
           delete_bitset: delete_bitset,
           positions_composite: positions_composite,
           schema: schema,
        })
    }

    pub fn field_reader(&self, field: Field) -> Result<Arc<FieldReader>> {
        if let Some(field_reader) = self.field_reader_cache.read()
            .unwrap() // TODO
            .get(&field) {
            return Ok(field_reader.clone());
        }

        // TODO better error
        let termdict_source = self.termdict_composite
            .open_read(field)
            .ok_or("Field not found")?;

        let postings_source = self.postings_composite
            .open_read(field)
            .ok_or("field not found")?;

        let positions_source = self.positions_composite
            .open_read(field)
            .ok_or("field not found")?;

        let field_reader = Arc::new(FieldReader::new(
            termdict_source,
            postings_source,
            positions_source,
            self.delete_bitset.clone(),
            self.schema.clone(),
        )?);

        self.field_reader_cache
            .write()
            .unwrap() // TODO
            .insert(field, field_reader.clone());
        Ok(field_reader)
    }

    /// Returns the document (or to be accurate, its stored field)
    /// bearing the given doc id.
    /// This method is slow and should seldom be called from
    /// within a collector.
    pub fn doc(&self, doc_id: DocId) -> Result<Document> {
        self.store_reader.get(doc_id)
    }


    /// Returns the segment id
    pub fn segment_id(&self) -> SegmentId {
        self.segment_id
    }

    /// Returns the bitset representing
    /// the documents that have been deleted.
    pub fn delete_bitset(&self) -> &DeleteBitSet {
        &self.delete_bitset
    }


    /// Returns true iff the `doc` is marked
    /// as deleted.
    pub fn is_deleted(&self, doc: DocId) -> bool {
        self.delete_bitset.is_deleted(doc)
    }
}


impl fmt::Debug for SegmentReader {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "SegmentReader({:?})", self.segment_id)
    }
}
