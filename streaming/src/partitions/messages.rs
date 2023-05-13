use crate::message::Message;
use crate::partitions::partition::Partition;
use crate::segments::segment::Segment;
use crate::utils::timestamp;
use ringbuffer::{RingBuffer, RingBufferExt, RingBufferWrite};
use shared::error::Error;
use std::sync::Arc;
use tracing::{error, trace};

const EMPTY_MESSAGES: Vec<Arc<Message>> = vec![];

impl Partition {
    pub async fn get_messages_by_timestamp(
        &self,
        timestamp: u64,
        count: u32,
    ) -> Result<Vec<Arc<Message>>, Error> {
        trace!(
            "Getting messages by timestamp: {} for partition: {}...",
            timestamp,
            self.id
        );
        if self.segments.is_empty() {
            return Ok(EMPTY_MESSAGES);
        }

        let mut maybe_start_offset = None;
        for segment in self.segments.iter() {
            if segment.time_indexes.is_empty() {
                continue;
            }

            let first_timestamp = segment.time_indexes.first().unwrap().timestamp;
            let last_timestamp = segment.time_indexes.last().unwrap().timestamp;
            if timestamp < first_timestamp || timestamp > last_timestamp {
                continue;
            }

            let relative_start_offset = segment
                .time_indexes
                .iter()
                .find(|time_index| time_index.timestamp >= timestamp)
                .map(|time_index| time_index.relative_offset)
                .unwrap_or(0);

            let start_offset = segment.start_offset + relative_start_offset as u64;
            maybe_start_offset = Some(start_offset);
            trace!(
                "Found start offset: {} for timestamp: {}.",
                start_offset,
                timestamp
            );

            break;
        }

        if maybe_start_offset.is_none() {
            trace!("Start offset for timestamp: {} was not found.", timestamp);
            return Ok(EMPTY_MESSAGES);
        }

        self.get_messages_by_offset(maybe_start_offset.unwrap(), count)
            .await
    }

    pub async fn get_messages_by_offset(
        &self,
        start_offset: u64,
        count: u32,
    ) -> Result<Vec<Arc<Message>>, Error> {
        trace!(
            "Getting messages for start offset: {} for partition: {}...",
            start_offset,
            self.id
        );
        if self.segments.is_empty() {
            return Ok(EMPTY_MESSAGES);
        }

        let end_offset = self.get_end_offset(start_offset, count);
        let messages = self.try_get_messages_from_cache(start_offset, end_offset);
        if let Some(messages) = messages {
            return Ok(messages);
        }

        let segments = self.filter_segments_by_offsets(start_offset, end_offset);
        match segments.len() {
            0 => Ok(EMPTY_MESSAGES),
            1 => segments[0].get_messages(start_offset, count).await,
            _ => Self::get_messages_from_segments(segments, start_offset, count).await,
        }
    }

    pub async fn get_first_messages(&self, count: u32) -> Result<Vec<Arc<Message>>, Error> {
        self.get_messages_by_offset(0, count).await
    }

    pub async fn get_last_messages(&self, count: u32) -> Result<Vec<Arc<Message>>, Error> {
        let mut count = count as u64;
        if count > self.current_offset + 1 {
            count = self.current_offset + 1
        }

        let start_offset = 1 + self.current_offset - count;
        self.get_messages_by_offset(start_offset, count as u32)
            .await
    }

    pub async fn get_next_messages(
        &self,
        consumer_id: u32,
        count: u32,
    ) -> Result<Vec<Arc<Message>>, Error> {
        let offset = self.consumer_offsets.get(&consumer_id);
        if offset.is_none() {
            trace!(
                "Consumer: {} hasn't stored offset for partition: {}, returning the first messages...",
                consumer_id,
                self.id
            );
            return self.get_first_messages(count).await;
        }

        let offset = offset.unwrap().offset;
        if offset == self.current_offset {
            trace!(
                "Consumer: {} has the latest offset for partition: {}, returning empty messages...",
                consumer_id,
                self.id
            );
            return Ok(EMPTY_MESSAGES);
        }

        self.get_messages_by_offset(offset, count).await
    }

    fn get_end_offset(&self, offset: u64, count: u32) -> u64 {
        let mut end_offset = offset + (count - 1) as u64;
        let segment = self.segments.last().unwrap();
        let max_offset = segment.current_offset;
        if end_offset > max_offset {
            end_offset = max_offset;
        }

        end_offset
    }

    fn filter_segments_by_offsets(&self, offset: u64, end_offset: u64) -> Vec<&Segment> {
        self.segments
            .iter()
            .filter(|segment| {
                (segment.start_offset >= offset && segment.current_offset <= end_offset)
                    || (segment.start_offset <= offset && segment.current_offset >= offset)
                    || (segment.start_offset <= end_offset && segment.current_offset >= end_offset)
            })
            .collect::<Vec<&Segment>>()
    }

    async fn get_messages_from_segments(
        segments: Vec<&Segment>,
        offset: u64,
        count: u32,
    ) -> Result<Vec<Arc<Message>>, Error> {
        let mut messages = Vec::with_capacity(segments.len());
        for segment in segments {
            let segment_messages = segment.get_messages(offset, count).await?;
            for message in segment_messages {
                messages.push(message);
            }
        }

        Ok(messages)
    }

    fn try_get_messages_from_cache(
        &self,
        start_offset: u64,
        end_offset: u64,
    ) -> Option<Vec<Arc<Message>>> {
        if self.messages.is_empty() {
            return None;
        }

        let first_buffered_offset = self.messages[0].offset;
        trace!(
            "First buffered offset: {} for partition: {}",
            first_buffered_offset,
            self.id
        );

        if start_offset >= first_buffered_offset {
            return Some(self.load_messages_from_cache(start_offset, end_offset));
        }

        None
    }

    fn load_messages_from_cache(&self, start_offset: u64, end_offset: u64) -> Vec<Arc<Message>> {
        trace!(
            "Loading messages from cache, start offset: {}, end offset: {}...",
            start_offset,
            end_offset
        );

        let messages_count = (1 + end_offset - start_offset) as usize;
        let messages = self
            .messages
            .iter()
            .filter(|message| message.offset >= start_offset && message.offset <= end_offset)
            .map(Arc::clone)
            .collect::<Vec<Arc<Message>>>();

        if messages.len() != messages_count {
            error!(
                "Loaded {} messages from cache, expected {}.",
                messages.len(),
                messages_count
            );
        }

        trace!(
            "Loaded {} messages from cache, start offset: {}, end offset: {}...",
            messages.len(),
            start_offset,
            end_offset
        );

        messages
    }

    pub async fn append_messages(&mut self, messages: Vec<Message>) -> Result<(), Error> {
        let segment = self.segments.last_mut();
        if segment.is_none() {
            return Err(Error::SegmentNotFound);
        }

        let mut segment = segment.unwrap();
        if segment.is_closed {
            let start_offset = segment.end_offset + 1;
            trace!(
                "Current segment is closed, creating new segment with start offset: {} for partition with ID: {}...",
                start_offset, self.id
            );
            self.process_new_segment(start_offset).await?;
            segment = self.segments.last_mut().unwrap();
        }

        let messages_count = messages.len() as u32;
        trace!(
            "Appending {} messages to segment with start offset: {} for partition with ID: {}...",
            messages_count,
            segment.start_offset,
            self.id
        );

        for mut message in messages {
            if self.should_increment_offset {
                self.current_offset += 1;
            } else {
                self.should_increment_offset = true;
            }
            trace!(
                "Appending the message with offset: {} to segment with start offset: {} for partition with ID: {}...",
                self.current_offset,
                segment.start_offset,
                self.id
            );

            message.offset = self.current_offset;
            message.timestamp = timestamp::get();
            let message = Arc::new(message);
            segment.append_message(message.clone()).await?;
            self.messages.push(message);

            trace!(
                "Appended the message with offset: {} to segment with start offset: {} for partition with ID: {}.",
                self.current_offset,
                segment.start_offset,
                self.id
            );
        }

        trace!(
            "Appended {} messages to segment with start offset: {} for partition with ID: {}.",
            messages_count,
            segment.start_offset,
            self.id
        );

        self.unsaved_messages_count += messages_count;
        if self.unsaved_messages_count >= self.config.messages_required_to_save || segment.is_full()
        {
            trace!(
            "Segment with start offset: {} for partition with ID: {} will be persisted on disk...",
            segment.start_offset,
            self.id
        );
            segment.persist_messages().await?;
            self.unsaved_messages_count = 0;
        }

        Ok(())
    }

    async fn process_new_segment(&mut self, start_offset: u64) -> Result<(), Error> {
        trace!(
            "Current segment is full, creating new segment for partition with ID: {}",
            self.id
        );
        let mut new_segment = Segment::create(
            self.id,
            start_offset,
            &self.path,
            self.config.segment.clone(),
        );
        new_segment.persist().await?;
        self.segments.push(new_segment);

        Ok(())
    }
}