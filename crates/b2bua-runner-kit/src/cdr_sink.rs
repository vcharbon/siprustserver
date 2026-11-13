//! The CDR sink a runner composes from env: the RabbitMQ sink
//! (`B2BUA_CDR_RABBITMQ_*`, [`RabbitMqCdrSettings`](crate::RabbitMqCdrSettings))
//! or the JSON-lines file sink (`B2BUA_CDR_FILE`, [`FileCdrWriter`]), one at
//! most, passed to [`RunnerBase::deps`].

use std::env;
use std::sync::Arc;

use b2bua::cdr::{CdrEncoder, CdrWriter};

use crate::cdr_file::{file_cdr_writer_from_lookup, FileCdrWriter};
use crate::cdr_rabbitmq::{
    rabbitmq_cdr_writer_from_lookup, rabbitmq_cdr_writer_from_lookup_with_encoder,
    RabbitMqCdrWriter,
};
use crate::RunnerBase;

impl RunnerBase {
    /// The CDR sink the env selects: the file `B2BUA_CDR_FILE` names, else
    /// the RabbitMQ sink of [`Self::rabbitmq_cdr_sink_from_env`]; `None` when
    /// neither is set. `Err` is the boot refusal of a file that cannot be
    /// opened, of both set, or of a malformed `B2BUA_CDR_RABBITMQ_*` value.
    pub fn cdr_sink_from_env(&self) -> Result<Option<Arc<dyn CdrWriter>>, String> {
        let file = file_cdr_writer_from_lookup(|k| env::var(k).ok(), &self.metrics)
            .map_err(|e| format!("invalid B2BUA config: {e}"))?;
        match file {
            Some(writer) => Ok(Some(self.wired_file_sink(writer))),
            None => self.rabbitmq_cdr_sink_from_env(),
        }
    }

    /// The RabbitMQ CDR sink the env selects ([`RabbitMqCdrSettings`](crate::RabbitMqCdrSettings)),
    /// publishing the default [`JsonRecordEncoder`](b2bua::cdr::JsonRecordEncoder)
    /// record and recording into [`Self::metrics`]; `None` when the URL is
    /// unset. `Err` is the boot refusal of a malformed `B2BUA_CDR_RABBITMQ_*`
    /// value.
    pub fn rabbitmq_cdr_sink_from_env(&self) -> Result<Option<Arc<dyn CdrWriter>>, String> {
        let writer = rabbitmq_cdr_writer_from_lookup(|k| env::var(k).ok(), &self.metrics)
            .map_err(|e| format!("invalid B2BUA config: {e}"))?;
        Ok(writer.map(|w| self.wired_cdr_sink(w)))
    }

    /// [`Self::rabbitmq_cdr_sink_from_env`] publishing the bytes `encoder`
    /// produces: the runner chooses the record format, the kit owns delivery.
    pub fn rabbitmq_cdr_sink_from_env_with_encoder(
        &self,
        encoder: Arc<dyn CdrEncoder>,
    ) -> Result<Option<Arc<dyn CdrWriter>>, String> {
        let lookup = |k: &str| env::var(k).ok();
        let writer = rabbitmq_cdr_writer_from_lookup_with_encoder(lookup, encoder, &self.metrics)
            .map_err(|e| format!("invalid B2BUA config: {e}"))?;
        Ok(writer.map(|w| self.wired_cdr_sink(w)))
    }

    fn wired_cdr_sink(&self, writer: RabbitMqCdrWriter) -> Arc<dyn CdrWriter> {
        let settings = writer.settings();
        tracing::info!(
            service = %self.name,
            sink = "rabbitmq",
            queue = %settings.queue,
            declare = ?settings.declare,
            buffer = self.env.cdr_queue,
            "CDR sink wired"
        );
        Arc::new(writer)
    }

    fn wired_file_sink(&self, writer: FileCdrWriter) -> Arc<dyn CdrWriter> {
        tracing::info!(
            service = %self.name,
            sink = "file",
            file = %writer.path().display(),
            buffer = self.env.cdr_queue,
            "CDR sink wired"
        );
        Arc::new(writer)
    }
}
