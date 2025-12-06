#![allow(clippy::missing_errors_doc)]

use std::borrow::Cow;
use std::cell::Cell;
use std::sync::Arc;

use arrayvec::ArrayVec;
use chrono::{DateTime, Utc};
use nonmax::{NonMaxU8, NonMaxU16};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{HeaderMap as Headers, HeaderValue};
use reqwest::{Response as ReqwestResponse, StatusCode};
use serde::de::DeserializeOwned;
use serde::ser::SerializeSeq as _;
use serde_json::{from_value, to_string, to_vec};
use to_arraystring::ToArrayString as _;
#[cfg(feature = "tracing_instrument")]
use tracing::instrument;
use tracing::{debug, warn};

use super::multipart::{Multipart, MultipartUpload};
use super::request::Request;
use super::routing::Route;
use super::{
    ErrorResponse,
    GuildPagination,
    HttpError,
    LightMethod,
    MessagePagination,
    UserPagination,
};
use crate::all::Http;
use crate::all::CreateAttachment;
use crate::constants;
use crate::internal::prelude::*;
use crate::model::prelude::*;

pub type ResultJson = Result<serde_json::Value>;

// NOTE: This cannot be passed in from outside, due to `Cell` being !Send.
struct SerializeIter<I>(Cell<Option<I>>);

impl<I> SerializeIter<I> {
    pub fn new(iter: I) -> Self {
        Self(Cell::new(Some(iter)))
    }
}

impl<Iter, Item> serde::Serialize for SerializeIter<Iter>
where
    Iter: Iterator<Item = Item>,
    Item: serde::Serialize,
{
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let Some(iter) = self.0.take() else {
            return serializer.serialize_seq(Some(0))?.end();
        };

        serializer.collect_seq(iter)
    }
}

fn reason_into_header(reason: &str) -> Headers {
    let mut headers = Headers::new();

    // "The X-Audit-Log-Reason header supports 1-512 URL-encoded UTF-8 characters."
    // https://discord.com/developers/docs/resources/audit-log#audit-log-entry-object
    let header_value = match Cow::from(utf8_percent_encode(reason, NON_ALPHANUMERIC)) {
        Cow::Borrowed(value) => HeaderValue::from_str(value),
        Cow::Owned(value) => HeaderValue::try_from(value),
    }
    .expect("Invalid header value even after percent encode");

    headers.insert("X-Audit-Log-Reason", header_value);
    headers
}

/// A low-level client for sending requests to Discord's HTTP REST API.
///
/// **Note**: For all member functions that return a [`Result`], the Error kind will be either
/// [`Error::Http`] or [`Error::Json`].
#[derive(Debug)]
pub struct JsonHttp {
    http: Arc<Http>,
}

impl JsonHttp {
    /// Construct an authorized HTTP client.
    #[must_use]
    pub fn new(http: Arc<Http>) -> Self {
        Self { http }
    }

    pub fn application_id(&self) -> Option<ApplicationId> {
        self.http.application_id()
    }

    fn try_application_id(&self) -> Result<ApplicationId> {
        self.application_id().ok_or_else(|| HttpError::ApplicationIdMissing.into())
    }

    pub fn set_application_id(&self, application_id: ApplicationId) {
        self.http.set_application_id(application_id);
    }

    /// Adds a [`User`] to a [`Guild`] with a valid OAuth2 access token.
    ///
    /// Returns the created [`Member`] object, or nothing if the user is already a guild member.
    pub async fn add_guild_member(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        let body = to_vec(map)?;

        let response = self
            .request(Request {
                body: Some(body),
                multipart: None,
                headers: None,
                method: LightMethod::Put,
                route: Route::GuildMember {
                    guild_id,
                    user_id,
                },
                params: None,
            })
            .await?;

        if response.status() == 204 { Ok(serde_json::Value::Null) } else { Ok(response.json().await?) }
    }

    /// Adds a single [`Role`] to a [`Member`] in a [`Guild`].
    pub async fn add_member_role(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        role_id: RoleId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Put,
            route: Route::GuildMemberRole {
                guild_id,
                role_id,
                user_id,
            },
            params: None,
        })
        .await
    }

    /// Bans a [`User`] from a [`Guild`], removing their messages sent in the last X number of
    /// seconds.
    ///
    /// Passing a `delete_message_seconds` of `0` is equivalent to not removing any messages. Up to
    /// `604800` seconds (or 7 days) worth of messages may be deleted.
    pub async fn ban_user(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        delete_message_seconds: u32,
        reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: reason.map(reason_into_header),
            method: LightMethod::Put,
            route: Route::GuildBan {
                guild_id,
                user_id,
            },
            params: Some(&[("delete_message_seconds", &delete_message_seconds.to_arraystring())]),
        })
        .await
    }

    /// Bans multiple users from a [`Guild`], optionally removing their messages.
    ///
    /// See the [Discord docs](https://discord.com/developers/docs/resources/guild#bulk-guild-ban)
    /// for more information.
    pub async fn bulk_ban_users(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
        reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::GuildBulkBan {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Broadcasts that the current user is typing in the given [`Channel`].
    pub async fn broadcast_typing(&self, channel_id: GenericChannelId) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::ChannelTyping {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Creates a [`GuildChannel`] in the [`Guild`] given its Id.
    pub async fn create_channel(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::GuildChannels {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Creates a stage instance.
    pub async fn create_stage_instance(
        &self,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::StageInstances,
            params: None,
        })
        .await
    }

    /// Creates a thread channel in the [`GuildChannel`] given its Id, with a base message Id.
    pub async fn create_thread_from_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::ChannelMessageThreads {
                channel_id,
                message_id,
            },
            params: None,
        })
        .await
    }

    /// Creates a thread channel not attached to a message in the [`GuildChannel`] given its Id.
    pub async fn create_thread(
        &self,
        channel_id: ChannelId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::ChannelThreads {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Creates a forum post channel in the [`GuildChannel`] given its Id.
    pub async fn create_forum_post(
        &self,
        channel_id: ChannelId,
        map: &impl serde::Serialize,
        files: Vec<CreateAttachment<'_>>,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: Some(Multipart {
                upload: MultipartUpload::Attachments(files),
                payload_json: Some(to_string(map)?),
                fields: vec![],
            }),
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::ChannelForumPosts {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Creates an emoji in the given [`Guild`] with the given data.
    pub async fn create_emoji(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::GuildEmojis {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Creates an application emoji with the given data.
    ///
    /// See [`Context::create_application_emoji`] for required fields.
    ///
    /// [`Context::create_application_emoji`]: crate::gateway::client::Context::create_application_emoji
    pub async fn create_application_emoji(&self, map: &impl serde::Serialize) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::Emojis {
                application_id: self.try_application_id()?,
            },
            params: None,
        })
        .await
    }

    /// Create a follow-up message for an Interaction.
    ///
    /// Functions the same as [`Self::execute_webhook`]
    pub async fn create_followup_message(
        &self,
        interaction_token: &str,
        map: &impl serde::Serialize,
        files: Vec<CreateAttachment<'_>>,
    ) -> ResultJson {
        let mut request = Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::WebhookFollowupMessages {
                application_id: self.try_application_id()?,
                token: interaction_token,
            },
            params: None,
        };

        if files.is_empty() {
            request.body = Some(to_vec(map)?);
        } else {
            request.multipart = Some(Multipart {
                upload: MultipartUpload::Attachments(files),
                payload_json: Some(to_string(map)?),
                fields: vec![],
            });
        }

        self.fire(request).await
    }

    /// Creates a new global command.
    pub async fn create_global_command(&self, map: &impl serde::Serialize) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::Commands {
                application_id: self.try_application_id()?,
            },
            params: None,
        })
        .await
    }

    /// Creates new global application commands.
    pub async fn create_global_commands(
        &self,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Put,
            route: Route::Commands {
                application_id: self.try_application_id()?,
            },
            params: None,
        })
        .await
    }

    /// Creates new guild application commands.
    pub async fn create_guild_commands(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Put,
            route: Route::GuildCommands {
                application_id: self.try_application_id()?,
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Creates a guild with the data provided.
    ///
    /// Only a [`PartialGuild`] will be immediately returned, and a full [`Guild`] will be received
    /// over a [`Shard`], if at least one is running.
    ///
    /// [`Shard`]: crate::gateway::Shard
    #[deprecated = "This endpoint has been deprecated by Discord and will stop functioning after July 15, 2025. For more information, see: https://discord.com/developers/docs/change-log#deprecating-guild-creation-by-apps"]
    pub async fn create_guild(&self, map: &impl serde::Serialize) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::Guilds,
            params: None,
        })
        .await
    }

    /// Creates a new guild command.
    ///
    /// New guild commands will be available in the guild immediately.
    pub async fn create_guild_command(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::GuildCommands {
                application_id: self.try_application_id()?,
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Creates an [`Integration`] for a [`Guild`].
    pub async fn create_guild_integration(
        &self,
        guild_id: GuildId,
        integration_id: IntegrationId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::GuildIntegration {
                guild_id,
                integration_id,
            },
            params: None,
        })
        .await
    }

    /// Creates a response to an [`Interaction`] from the gateway.
    pub async fn create_interaction_response(
        &self,
        interaction_id: InteractionId,
        interaction_token: &str,
        map: &impl serde::Serialize,
        files: Vec<CreateAttachment<'_>>,
    ) -> Result<()> {
        let mut request = Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::InteractionResponse {
                interaction_id,
                token: interaction_token,
            },
            params: None,
        };

        if files.is_empty() {
            request.body = Some(to_vec(map)?);
        } else {
            request.multipart = Some(Multipart {
                upload: MultipartUpload::Attachments(files),
                payload_json: Some(to_string(map)?),
                fields: vec![],
            });
        }

        self.wind(request).await
    }

    /// Creates a [`RichInvite`] for the given [channel][`GuildChannel`].
    pub async fn create_invite(
        &self,
        channel_id: ChannelId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::ChannelInvites {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Creates a permission override for a member or a role in a channel.
    pub async fn create_permission(
        &self,
        channel_id: ChannelId,
        target_id: TargetId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        let body = to_vec(map)?;

        self.wind(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Put,
            route: Route::ChannelPermission {
                channel_id,
                target_id,
            },
            params: None,
        })
        .await
    }

    /// Creates a private channel with a user.
    pub async fn create_private_channel(
        &self,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::UserMeDmChannels,
            params: None,
        })
        .await
    }

    /// Reacts to a message.
    pub async fn create_reaction(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
        reaction_type: &ReactionType,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Put,
            route: Route::ChannelMessageReactionMe {
                channel_id,
                message_id,
                reaction: &reaction_type.as_data(),
            },
            params: None,
        })
        .await
    }

    /// Creates a role.
    pub async fn create_role(
        &self,
        guild_id: GuildId,
        body: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let mut value: Value = self
            .fire(Request {
                body: Some(to_vec(body)?),
                multipart: None,
                headers: audit_log_reason.map(reason_into_header),
                method: LightMethod::Post,
                route: Route::GuildRoles {
                    guild_id,
                },
                params: None,
            })
            .await?;

        if let Some(map) = value.as_object_mut() {
            map.insert("guild_id".to_string(), guild_id.get().into());
        }

        from_value(value).map_err(From::from)
    }

    /// Creates a Guild Scheduled Event.
    pub async fn create_scheduled_event(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;
        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::GuildScheduledEvents {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Creates a sticker.
    pub async fn create_sticker(
        &self,
        guild_id: GuildId,
        fields: Vec<(Cow<'static, str>, Cow<'static, str>)>,
        file: CreateAttachment<'_>,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: Some(Multipart {
                upload: MultipartUpload::File(file),
                payload_json: None,
                fields,
            }),
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::GuildStickers {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Creates a webhook for the given [`GuildChannel`]'s Id, passing in the given data.
    pub async fn create_webhook(
        &self,
        channel_id: ChannelId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::ChannelWebhooks {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a private channel or a channel in a guild.
    pub async fn delete_channel(
        &self,
        channel_id: GenericChannelId,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::Channel {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a stage instance.
    pub async fn delete_stage_instance(
        &self,
        channel_id: ChannelId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::StageInstance {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes an emoji from a guild.
    pub async fn delete_emoji(
        &self,
        guild_id: GuildId,
        emoji_id: EmojiId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::GuildEmoji {
                guild_id,
                emoji_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes an application emoji.
    pub async fn delete_application_emoji(&self, emoji_id: EmojiId) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::Emoji {
                application_id: self.try_application_id()?,
                emoji_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a follow-up message for an interaction.
    pub async fn delete_followup_message(
        &self,
        interaction_token: &str,
        message_id: MessageId,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::WebhookFollowupMessage {
                application_id: self.try_application_id()?,
                token: interaction_token,
                message_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a global command.
    pub async fn delete_global_command(&self, command_id: CommandId) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::Command {
                application_id: self.try_application_id()?,
                command_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a guild, only if connected account owns it.
    pub async fn delete_guild(&self, guild_id: GuildId) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::Guild {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a guild command.
    pub async fn delete_guild_command(
        &self,
        guild_id: GuildId,
        command_id: CommandId,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::GuildCommand {
                application_id: self.try_application_id()?,
                guild_id,
                command_id,
            },
            params: None,
        })
        .await
    }

    /// Removes an integration from a guild.
    pub async fn delete_guild_integration(
        &self,
        guild_id: GuildId,
        integration_id: IntegrationId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::GuildIntegration {
                guild_id,
                integration_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes an invite by code.
    pub async fn delete_invite(
        &self,
        code: &str,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::Invite {
                code,
            },
            params: None,
        })
        .await
    }

    /// Deletes a message if created by us or we have specific permissions.
    pub async fn delete_message(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::ChannelMessage {
                channel_id,
                message_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a bunch of messages, only works for bots.
    pub async fn delete_messages(
        &self,
        channel_id: GenericChannelId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::ChannelMessagesBulkDelete {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes all of the [`Reaction`]s associated with a [`Message`].
    pub async fn delete_message_reactions(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::ChannelMessageReactions {
                channel_id,
                message_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes all the reactions for a given emoji on a message.
    pub async fn delete_message_reaction_emoji(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
        reaction_type: &ReactionType,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::ChannelMessageReactionEmoji {
                channel_id,
                message_id,
                reaction: &reaction_type.as_data(),
            },
            params: None,
        })
        .await
    }

    /// Deletes the initial interaction response.
    pub async fn delete_original_interaction_response(
        &self,
        interaction_token: &str,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::WebhookOriginalInteractionResponse {
                application_id: self.try_application_id()?,
                token: interaction_token,
            },
            params: None,
        })
        .await
    }

    /// Deletes a permission override from a role or a member in a channel.
    pub async fn delete_permission(
        &self,
        channel_id: ChannelId,
        target_id: TargetId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::ChannelPermission {
                channel_id,
                target_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a user's reaction from a message.
    pub async fn delete_reaction(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
        user_id: UserId,
        reaction_type: &ReactionType,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::ChannelMessageReaction {
                channel_id,
                message_id,
                user_id,
                reaction: &reaction_type.as_data(),
            },
            params: None,
        })
        .await
    }

    /// Deletes a reaction by the current user from a message.
    pub async fn delete_reaction_me(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
        reaction_type: &ReactionType,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::ChannelMessageReactionMe {
                channel_id,
                message_id,
                reaction: &reaction_type.as_data(),
            },
            params: None,
        })
        .await
    }

    /// Deletes a role from a server. Can't remove the default everyone role.
    pub async fn delete_role(
        &self,
        guild_id: GuildId,
        role_id: RoleId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::GuildRole {
                guild_id,
                role_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a [Scheduled Event] from a server.
    pub async fn delete_scheduled_event(
        &self,
        guild_id: GuildId,
        event_id: ScheduledEventId,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::GuildScheduledEvent {
                guild_id,
                event_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a sticker from a server.
    pub async fn delete_sticker(
        &self,
        guild_id: GuildId,
        sticker_id: StickerId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::GuildSticker {
                guild_id,
                sticker_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a currently active test entitlement. Discord will act as though the corresponding
    /// user/guild *no longer has* an entitlement to the corresponding SKU.
    pub async fn delete_test_entitlement(&self, entitlement_id: EntitlementId) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::Entitlement {
                application_id: self.try_application_id()?,
                entitlement_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a [`Webhook`] given its Id.
    pub async fn delete_webhook(
        &self,
        webhook_id: WebhookId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::Webhook {
                webhook_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a [`Webhook`] given its Id and unique token.
    ///
    /// This method does _not_ require authentication.
    pub async fn delete_webhook_with_token(
        &self,
        webhook_id: WebhookId,
        token: &str,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::WebhookWithToken {
                webhook_id,
                token,
            },
            params: None,
        })
        .await
    }

    /// Changes channel information.
    pub async fn edit_channel(
        &self,
        channel_id: GenericChannelId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::Channel {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Edits a stage instance.
    pub async fn edit_stage_instance(
        &self,
        channel_id: ChannelId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::StageInstance {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Changes guild emoji information.
    pub async fn edit_emoji(
        &self,
        guild_id: GuildId,
        emoji_id: EmojiId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::GuildEmoji {
                guild_id,
                emoji_id,
            },
            params: None,
        })
        .await
    }

    /// Changes application emoji information.
    ///
    /// See [`Context::edit_application_emoji`] for required fields.
    ///
    /// [`Context::edit_application_emoji`]: crate::gateway::client::Context::edit_application_emoji
    pub async fn edit_application_emoji(
        &self,
        emoji_id: EmojiId,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::Emoji {
                application_id: self.try_application_id()?,
                emoji_id,
            },
            params: None,
        })
        .await
    }

    /// Edits a follow-up message for an interaction.
    pub async fn edit_followup_message(
        &self,
        interaction_token: &str,
        message_id: MessageId,
        map: &impl serde::Serialize,
        new_attachments: Vec<CreateAttachment<'_>>,
    ) -> ResultJson {
        let mut request = Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::WebhookFollowupMessage {
                application_id: self.try_application_id()?,
                token: interaction_token,
                message_id,
            },
            params: None,
        };

        if new_attachments.is_empty() {
            request.body = Some(to_vec(map)?);
        } else {
            request.multipart = Some(Multipart {
                upload: MultipartUpload::Attachments(new_attachments),
                payload_json: Some(to_string(map)?),
                fields: vec![],
            });
        }

        self.fire(request).await
    }

    /// Get a follow-up message for an interaction.
    pub async fn get_followup_message(
        &self,
        interaction_token: &str,
        message_id: MessageId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::WebhookFollowupMessage {
                application_id: self.try_application_id()?,
                token: interaction_token,
                message_id,
            },
            params: None,
        })
        .await
    }

    /// Edits a global command.
    pub async fn edit_global_command(
        &self,
        command_id: CommandId,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::Command {
                application_id: self.try_application_id()?,
                command_id,
            },
            params: None,
        })
        .await
    }

    /// Changes guild information.
    pub async fn edit_guild(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::Guild {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Edits a guild command.
    pub async fn edit_guild_command(
        &self,
        guild_id: GuildId,
        command_id: CommandId,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::GuildCommand {
                application_id: self.try_application_id()?,
                guild_id,
                command_id,
            },
            params: None,
        })
        .await
    }

    /// Edits a guild command permissions.
    pub async fn edit_guild_command_permissions(
        &self,
        guild_id: GuildId,
        command_id: CommandId,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Put,
            route: Route::GuildCommandPermissions {
                application_id: self.try_application_id()?,
                guild_id,
                command_id,
            },
            params: None,
        })
        .await
    }

    /// Edits the positions of a guild's channels.
    pub async fn edit_guild_channel_positions(
        &self,
        guild_id: GuildId,
        value: impl Iterator<Item: serde::Serialize>,
    ) -> Result<()> {
        let body = to_vec(&SerializeIter::new(value))?;

        self.wind(Request {
            body: Some(body),
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::GuildChannels {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Edits the MFA level of a guild. Requires guild ownership.
    pub async fn edit_guild_mfa_level(
        &self,
        guild_id: GuildId,
        value: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(value)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::GuildMfa {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Edits a [`Guild`]'s widget.
    pub async fn edit_guild_widget(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::GuildWidget {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Edits a guild welcome screen.
    pub async fn edit_guild_welcome_screen(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::GuildWelcomeScreen {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Does specific actions to a member.
    pub async fn edit_member(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        let mut value: Value = self
            .fire(Request {
                body: Some(body),
                multipart: None,
                headers: audit_log_reason.map(reason_into_header),
                method: LightMethod::Patch,
                route: Route::GuildMember {
                    guild_id,
                    user_id,
                },
                params: None,
            })
            .await?;

        if let Some(map) = value.as_object_mut() {
            map.insert("guild_id".to_string(), guild_id.to_string().into());
        }

        Ok(value)
    }

    /// Edits a message by Id.
    ///
    /// **Note**: Only the author of a message can modify it.
    pub async fn edit_message(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
        map: &impl serde::Serialize,
        new_attachments: Vec<CreateAttachment<'_>>,
    ) -> ResultJson {
        let mut request = Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::ChannelMessage {
                channel_id,
                message_id,
            },
            params: None,
        };

        if new_attachments.is_empty() {
            request.body = Some(to_vec(map)?);
        } else {
            request.multipart = Some(Multipart {
                upload: MultipartUpload::Attachments(new_attachments),
                payload_json: Some(to_string(map)?),
                fields: vec![],
            });
        }

        self.fire(request).await
    }

    /// Crossposts a message by Id.
    ///
    /// **Note**: Only available on news channels.
    pub async fn crosspost_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::ChannelMessageCrosspost {
                channel_id,
                message_id,
            },
            params: None,
        })
        .await
    }

    /// Edits the current member for the provided [`Guild`] via its Id.
    pub async fn edit_member_me(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::GuildMemberMe {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Edits the current member for the provided [`Guild`] via its Id.
    pub async fn edit_current_member(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(&map)?),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::GuildMemberMe {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Follow a News Channel to send messages to a target channel.
    pub async fn follow_news_channel(
        &self,
        news_channel_id: ChannelId,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(&map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::ChannelFollowNews {
                channel_id: news_channel_id,
            },
            params: None,
        })
        .await
    }

    /// Gets the initial interaction response.
    pub async fn get_original_interaction_response(
        &self,
        interaction_token: &str,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::WebhookOriginalInteractionResponse {
                application_id: self.try_application_id()?,
                token: interaction_token,
            },
            params: None,
        })
        .await
    }

    /// Edits the initial interaction response.
    pub async fn edit_original_interaction_response(
        &self,
        interaction_token: &str,
        map: &impl serde::Serialize,
        new_attachments: Vec<CreateAttachment<'_>>,
    ) -> ResultJson {
        let mut request = Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::WebhookOriginalInteractionResponse {
                application_id: self.try_application_id()?,
                token: interaction_token,
            },
            params: None,
        };

        if new_attachments.is_empty() {
            request.body = Some(to_vec(map)?);
        } else {
            request.multipart = Some(Multipart {
                upload: MultipartUpload::Attachments(new_attachments),
                payload_json: Some(to_string(map)?),
                fields: vec![],
            });
        }

        self.fire(request).await
    }

    /// Edits the current user's profile settings.
    pub async fn edit_profile(&self, map: &impl serde::Serialize) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::UserMe,
            params: None,
        })
        .await
    }

    /// Changes a role in a guild.
    pub async fn edit_role(
        &self,
        guild_id: GuildId,
        role_id: RoleId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let mut value: Value = self
            .fire(Request {
                body: Some(to_vec(map)?),
                multipart: None,
                headers: audit_log_reason.map(reason_into_header),
                method: LightMethod::Patch,
                route: Route::GuildRole {
                    guild_id,
                    role_id,
                },
                params: None,
            })
            .await?;

        if let Some(map) = value.as_object_mut() {
            map.insert("guild_id".to_string(), guild_id.get().into());
        }

        from_value(value).map_err(From::from)
    }

    /// Changes the positions of roles in a guild.
    pub async fn edit_role_positions(
        &self,
        guild_id: GuildId,
        positions: impl Iterator<Item: serde::Serialize>,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(&SerializeIter::new(positions))?;

        let mut value: Value = self
            .fire(Request {
                body: Some(body),
                multipart: None,
                headers: audit_log_reason.map(reason_into_header),
                method: LightMethod::Patch,
                route: Route::GuildRoles {
                    guild_id,
                },
                params: None,
            })
            .await?;

        if let Some(array) = value.as_array_mut() {
            for role in array {
                if let Some(map) = role.as_object_mut() {
                    map.insert("guild_id".to_string(), guild_id.get().into());
                }
            }
        }

        from_value(value).map_err(From::from)
    }

    /// Modifies a scheduled event.
    ///
    /// **Note**: Requires the [Manage Events] permission.
    ///
    /// [Manage Events]: Permissions::MANAGE_EVENTS
    pub async fn edit_scheduled_event(
        &self,
        guild_id: GuildId,
        event_id: ScheduledEventId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;
        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::GuildScheduledEvent {
                guild_id,
                event_id,
            },
            params: None,
        })
        .await
    }

    /// Changes a sticker in a guild.
    ///
    /// See [`GuildId::edit_sticker`] for permissions requirements.
    pub async fn edit_sticker(
        &self,
        guild_id: GuildId,
        sticker_id: StickerId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(&map)?;

        let mut value: Value = self
            .fire(Request {
                body: Some(body),
                multipart: None,
                headers: audit_log_reason.map(reason_into_header),
                method: LightMethod::Patch,
                route: Route::GuildSticker {
                    guild_id,
                    sticker_id,
                },
                params: None,
            })
            .await?;

        if let Some(map) = value.as_object_mut() {
            map.insert("guild_id".to_string(), guild_id.get().into());
        }

        from_value(value).map_err(From::from)
    }

    /// Changes another user's voice state in a stage channel.
    pub async fn edit_voice_state(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        map: &impl serde::Serialize,
    ) -> Result<()> {
        self.wind(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::GuildVoiceStates {
                guild_id,
                user_id,
            },
            params: None,
        })
        .await
    }

    /// Changes the current user's voice state in a stage channel.
    pub async fn edit_voice_state_me(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
    ) -> Result<()> {
        self.wind(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::GuildVoiceStateMe {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Changes a voice channel's status.
    pub async fn edit_voice_status(
        &self,
        channel_id: ChannelId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        let body = to_vec(map)?;

        self.wind(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Put,
            route: Route::ChannelVoiceStatus {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Edits a the webhook with the given data.
    pub async fn edit_webhook(
        &self,
        webhook_id: WebhookId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        self.fire(Request {
            body: Some(to_vec(map)?),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::Webhook {
                webhook_id,
            },
            params: None,
        })
        .await
    }

    /// Edits the webhook with the given data.
    pub async fn edit_webhook_with_token(
        &self,
        webhook_id: WebhookId,
        token: &str,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::WebhookWithToken {
                webhook_id,
                token,
            },
            params: None,
        })
        .await
    }

    /// Executes a webhook, posting a [`Message`] in the webhook's associated [`Channel`].
    pub async fn execute_webhook(
        &self,
        webhook_id: WebhookId,
        thread_id: Option<ThreadId>,
        token: &str,
        wait: bool,
        files: Vec<CreateAttachment<'_>>,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        self.execute_webhook_(webhook_id, thread_id, token, wait, files, map, false).await
    }

    /// Same as [`Self::execute_webhook`] but allows sending non interactive components on non
    /// app-owned webhooks.
    ///
    /// Refer to the [Discord docs] for more information on how this works.
    ///
    /// [Discord docs]: https://discord.com/developers/docs/resources/webhook#execute-webhook-query-string-params
    pub async fn execute_webhook_with_components(
        &self,
        webhook_id: WebhookId,
        thread_id: Option<ThreadId>,
        token: &str,
        wait: bool,
        files: Vec<CreateAttachment<'_>>,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        self.execute_webhook_(webhook_id, thread_id, token, wait, files, map, true).await
    }

    #[expect(clippy::too_many_arguments)]
    async fn execute_webhook_(
        &self,
        webhook_id: WebhookId,
        thread_id: Option<ThreadId>,
        token: &str,
        wait: bool,
        files: Vec<CreateAttachment<'_>>,
        map: &impl serde::Serialize,
        with_components: bool,
    ) -> ResultJson {
        let (thread_id_str, with_components_str);
        let wait_str = wait.to_arraystring();
        let mut params = ArrayVec::<_, 3>::new();

        params.push(("wait", wait_str.as_str()));
        if let Some(thread_id) = thread_id {
            thread_id_str = thread_id.to_arraystring();
            params.push(("thread_id", &thread_id_str));
        }

        if with_components {
            with_components_str = with_components.to_arraystring();
            params.push(("with_components", &with_components_str));
        }

        let mut request = Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::WebhookWithToken {
                webhook_id,
                token,
            },
            params: Some(&params),
        };

        if files.is_empty() {
            request.body = Some(to_vec(map)?);
        } else {
            request.multipart = Some(Multipart {
                upload: MultipartUpload::Attachments(files),
                payload_json: Some(to_string(map)?),
                fields: vec![],
            });
        }

        let response = self.request(request).await?;

        Ok(if response.status() == StatusCode::NO_CONTENT { serde_json::Value::Null } else { response.json().await? })
    }

    // Gets a webhook's message by Id
    pub async fn get_webhook_message(
        &self,
        webhook_id: WebhookId,
        thread_id: Option<ThreadId>,
        token: &str,
        message_id: MessageId,
    ) -> ResultJson {
        let thread_id_str;
        let mut params = None;

        if let Some(thread_id) = thread_id {
            thread_id_str = thread_id.to_arraystring();
            params = Some([("thread_id", thread_id_str.as_str())]);
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::WebhookMessage {
                webhook_id,
                token,
                message_id,
            },
            params: params.as_ref().map(<[_; 1]>::as_slice),
        })
        .await
    }

    /// Edits a webhook's message by Id.
    pub async fn edit_webhook_message(
        &self,
        webhook_id: WebhookId,
        thread_id: Option<ThreadId>,
        token: &str,
        message_id: MessageId,
        map: &impl serde::Serialize,
        new_attachments: Vec<CreateAttachment<'_>>,
    ) -> ResultJson {
        let thread_id_str;
        let mut params = None;

        if let Some(thread_id) = thread_id {
            thread_id_str = thread_id.to_arraystring();
            params = Some([("thread_id", thread_id_str.as_str())]);
        }

        let mut request = Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Patch,
            route: Route::WebhookMessage {
                webhook_id,
                token,
                message_id,
            },
            params: params.as_ref().map(<[_; 1]>::as_slice),
        };

        if new_attachments.is_empty() {
            request.body = Some(to_vec(map)?);
        } else {
            request.multipart = Some(Multipart {
                upload: MultipartUpload::Attachments(new_attachments),
                payload_json: Some(to_string(map)?),
                fields: vec![],
            });
        }

        self.fire(request).await
    }

    /// Deletes a webhook's message by Id.
    pub async fn delete_webhook_message(
        &self,
        webhook_id: WebhookId,
        thread_id: Option<ThreadId>,
        token: &str,
        message_id: MessageId,
    ) -> Result<()> {
        let thread_id_str;
        let mut params = None;

        if let Some(thread_id) = thread_id {
            thread_id_str = thread_id.to_arraystring();
            params = Some([("thread_id", thread_id_str.as_str())]);
        }

        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::WebhookMessage {
                webhook_id,
                token,
                message_id,
            },
            params: params.as_ref().map(<[_; 1]>::as_slice),
        })
        .await
    }

    /// Gets all the users that are banned in specific guild, with additional options for
    /// filtering.
    ///
    /// If `limit` is left unset, by default at most 1000 worths of data for banned users is
    /// returned.
    ///
    /// If `target` is set, then users will be filtered by Id, such that their Id comes before or
    /// after the provided [`UserId`] wrapped by the [`UserPagination`].
    ///
    /// [`UserId`]: crate::model::id::UserId
    pub async fn get_bans(
        &self,
        guild_id: GuildId,
        target: Option<UserPagination>,
        limit: Option<NonMaxU16>,
    ) -> ResultJson {
        let id_str;
        let limit_str;
        let mut params = ArrayVec::<_, 2>::new();

        if let Some(limit) = limit {
            limit_str = limit.get().to_arraystring();
            params.push(("limit", limit_str.as_str()));
        }

        if let Some(target) = target {
            let (name, id) = match target {
                UserPagination::After(id) => ("after", id),
                UserPagination::Before(id) => ("before", id),
            };

            id_str = id.to_arraystring();
            params.push((name, &id_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildBans {
                guild_id,
            },
            params: Some(&params),
        })
        .await
    }

    /// Gets a [`Ban`] for a specific user in a guild. Returns [`None`] if no ban was found
    /// matching both the [`GuildId`] and [`UserId`].
    ///
    /// **Note**: Requires that you have the [Ban Members] permission
    ///
    /// # Errors
    ///
    /// Returns [`Error::Http`] if the current user lacks permission.
    ///
    /// [Ban Members]: Permissions::BAN_MEMBERS
    pub async fn get_ban(&self, guild_id: GuildId, user_id: UserId) -> ResultJson {
        let result = self
            .request(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::GuildBan {
                    guild_id,
                    user_id,
                },
                params: None,
            })
            .await?;

        if result.status() == StatusCode::NOT_FOUND {
            return Ok(serde_json::Value::Null);
        }

        Ok(result.json().await?)
    }

    /// Gets all audit logs in a specific guild.
    pub async fn get_audit_logs(
        &self,
        guild_id: GuildId,
        action_type: Option<u16>,
        user_id: Option<UserId>,
        before: Option<AuditLogEntryId>,
        limit: Option<NonMaxU8>,
    ) -> ResultJson {
        let (action_type_str, before_str, limit_str, user_id_str);
        let mut params = ArrayVec::<_, 4>::new();
        if let Some(action_type) = action_type {
            action_type_str = action_type.to_arraystring();
            params.push(("action_type", action_type_str.as_str()));
        }
        if let Some(before) = before {
            before_str = before.to_arraystring();
            params.push(("before", &before_str));
        }
        if let Some(limit) = limit {
            limit_str = limit.get().to_arraystring();
            params.push(("limit", &limit_str));
        }
        if let Some(user_id) = user_id {
            user_id_str = user_id.to_arraystring();
            params.push(("user_id", &user_id_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildAuditLogs {
                guild_id,
            },
            params: Some(&params),
        })
        .await
    }

    /// Retrieves all auto moderation rules in a guild.
    pub async fn get_automod_rules(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildAutomodRules {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Retrieves an auto moderation rule in a guild.
    pub async fn get_automod_rule(
        &self,
        guild_id: GuildId,
        rule_id: RuleId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildAutomodRule {
                guild_id,
                rule_id,
            },
            params: None,
        })
        .await
    }

    /// Creates an auto moderation rule in a guild.
    pub async fn create_automod_rule(
        &self,
        guild_id: GuildId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::GuildAutomodRules {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Retrieves an auto moderation rule in a guild.
    pub async fn edit_automod_rule(
        &self,
        guild_id: GuildId,
        rule_id: RuleId,
        map: &impl serde::Serialize,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let body = to_vec(map)?;

        self.fire(Request {
            body: Some(body),
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Patch,
            route: Route::GuildAutomodRule {
                guild_id,
                rule_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes an auto moderation rule in a guild.
    pub async fn delete_automod_rule(
        &self,
        guild_id: GuildId,
        rule_id: RuleId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::GuildAutomodRule {
                guild_id,
                rule_id,
            },
            params: None,
        })
        .await
    }

    /// Gets current bot gateway.
    pub async fn get_bot_gateway(&self) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GatewayBot,
            params: None,
        })
        .await
    }

    /// Gets all invites for a channel.
    pub async fn get_channel_invites(&self, channel_id: ChannelId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::ChannelInvites {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Gets all thread members for a thread.
    pub async fn get_channel_thread_members(
        &self,
        thread_id: ThreadId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::ChannelThreadMembers {
                thread_id,
            },
            params: None,
        })
        .await
    }

    /// Gets all active threads from a guild.
    pub async fn get_guild_active_threads(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildThreadsActive {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets all archived public threads from a channel.
    pub async fn get_channel_archived_public_threads(
        &self,
        channel_id: ChannelId,
        before: Option<DateTime<Utc>>,
        limit: Option<u64>,
    ) -> ResultJson {
        let (before_str, limit_str);
        let mut params = ArrayVec::<_, 2>::new();
        if let Some(before) = before {
            before_str = before.to_string();
            params.push(("before", before_str.as_str()));
        }
        if let Some(limit) = limit {
            limit_str = limit.to_arraystring();
            params.push(("limit", &limit_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            method: LightMethod::Get,
            headers: None,
            route: Route::ChannelArchivedPublicThreads {
                channel_id,
            },
            params: Some(&params),
        })
        .await
    }

    /// Gets all archived private threads from a channel.
    pub async fn get_channel_archived_private_threads(
        &self,
        channel_id: ChannelId,
        before: Option<DateTime<Utc>>,
        limit: Option<u64>,
    ) -> ResultJson {
        let (before_str, limit_str);
        let mut params = ArrayVec::<_, 2>::new();
        if let Some(before) = before {
            before_str = before.to_string();
            params.push(("before", before_str.as_str()));
        }
        if let Some(limit) = limit {
            limit_str = limit.to_arraystring();
            params.push(("limit", &limit_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::ChannelArchivedPrivateThreads {
                channel_id,
            },
            params: Some(&params),
        })
        .await
    }

    /// Gets all archived private threads joined from a channel.
    pub async fn get_channel_joined_archived_private_threads(
        &self,
        channel_id: ChannelId,
        before: Option<ChannelId>,
        limit: Option<u64>,
    ) -> ResultJson {
        let (before_str, limit_str);
        let mut params = ArrayVec::<_, 2>::new();
        if let Some(before) = before {
            before_str = before.to_arraystring();
            params.push(("before", before_str.as_str()));
        }
        if let Some(limit) = limit {
            limit_str = limit.to_arraystring();
            params.push(("limit", &limit_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::ChannelJoinedPrivateThreads {
                channel_id,
            },
            params: Some(&params),
        })
        .await
    }

    /// Joins a thread channel.
    pub async fn join_thread_channel(&self, thread_id: ThreadId) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Put,
            route: Route::ChannelThreadMemberMe {
                thread_id,
            },
            params: None,
        })
        .await
    }

    /// Leaves a thread channel.
    pub async fn leave_thread_channel(&self, thread_id: ThreadId) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::ChannelThreadMemberMe {
                thread_id,
            },
            params: None,
        })
        .await
    }

    /// Adds a member to a thread channel.
    pub async fn add_thread_channel_member(
        &self,
        thread_id: ThreadId,
        user_id: UserId,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Put,
            route: Route::ChannelThreadMember {
                thread_id,
                user_id,
            },
            params: None,
        })
        .await
    }

    /// Removes a member from a thread channel.
    pub async fn remove_thread_channel_member(
        &self,
        thread_id: ThreadId,
        user_id: UserId,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::ChannelThreadMember {
                thread_id,
                user_id,
            },
            params: None,
        })
        .await
    }

    pub async fn get_thread_channel_member(
        &self,
        thread_id: ThreadId,
        user_id: UserId,
        with_member: bool,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::ChannelThreadMember {
                thread_id,
                user_id,
            },
            params: Some(&[("with_member", &with_member.to_arraystring())]),
        })
        .await
    }

    /// Retrieves the webhooks for the given [channel][`GuildChannel`]'s Id.
    pub async fn get_channel_webhooks(&self, channel_id: ChannelId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::ChannelWebhooks {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Gets channel information.
    pub async fn get_channel(&self, channel_id: GenericChannelId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Channel {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Gets all channels in a guild.
    pub async fn get_channels(
        &self,
        guild_id: GuildId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildChannels {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets a stage instance.
    pub async fn get_stage_instance(&self, channel_id: ChannelId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::StageInstance {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Get a list of users that voted for this specific answer.
    pub async fn get_poll_answer_voters(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
        answer_id: AnswerId,
        after: Option<UserId>,
        limit: Option<u8>,
    ) -> ResultJson {
        let (after_str, limit_str);
        let mut params = ArrayVec::<_, 2>::new();
        if let Some(after) = after {
            after_str = after.to_arraystring();
            params.push(("after", after_str.as_str()));
        }

        if let Some(limit) = limit {
            limit_str = limit.to_arraystring();
            params.push(("limit", &limit_str));
        }

        self
            .fire(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::ChannelPollGetAnswerVoters {
                    channel_id,
                    message_id,
                    answer_id,
                },
                params: Some(&params),
            })
            .await
    }

    pub async fn expire_poll(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::ChannelPollExpire {
                channel_id,
                message_id,
            },
            params: None,
        })
        .await
    }

    /// Gets information about the current application.
    ///
    /// **Note**: Only applications may use this endpoint.
    pub async fn get_current_application_info(&self) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::OAuth2ApplicationCurrent,
            params: None,
        })
        .await
    }

    /// Gets information about the user we're connected with.
    pub async fn get_current_user(&self) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::UserMe,
            params: None,
        })
        .await
    }

    /// Gets all emojis of a guild.
    pub async fn get_emojis(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildEmojis {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets information about an emoji in a guild.
    pub async fn get_emoji(&self, guild_id: GuildId, emoji_id: EmojiId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildEmoji {
                guild_id,
                emoji_id,
            },
            params: None,
        })
        .await
    }

    /// Gets all emojis for the current application.
    pub async fn get_application_emojis(&self) -> ResultJson {
        self
            .fire(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::Emojis {
                    application_id: self.try_application_id()?,
                },
                params: None,
            })
            .await
    }

    /// Gets information about an application emoji.
    pub async fn get_application_emoji(&self, emoji_id: EmojiId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Emoji {
                application_id: self.try_application_id()?,
                emoji_id,
            },
            params: None,
        })
        .await
    }

    #[expect(clippy::too_many_arguments)]
    /// Gets all entitlements for the current app, active and expired.
    pub async fn get_entitlements(
        &self,
        user_id: Option<UserId>,
        sku_ids: Option<&[SkuId]>,
        before: Option<EntitlementId>,
        after: Option<EntitlementId>,
        limit: Option<NonMaxU8>,
        guild_id: Option<GuildId>,
        exclude_ended: Option<bool>,
    ) -> ResultJson {
        let (user_id_str, sku_ids_str, before_str, after_str, limit_str, guild_id_str, exclude_str);
        let mut params = ArrayVec::<_, 7>::new();
        if let Some(user_id) = user_id {
            user_id_str = user_id.to_arraystring();
            params.push(("user_id", user_id_str.as_str()));
        }
        if let Some(sku_ids) = sku_ids {
            sku_ids_str = join_to_string(',', sku_ids);
            params.push(("sku_ids", &sku_ids_str));
        }
        if let Some(before) = before {
            before_str = before.to_arraystring();
            params.push(("before", &before_str));
        }
        if let Some(after) = after {
            after_str = after.to_arraystring();
            params.push(("after", &after_str));
        }
        if let Some(limit) = limit {
            limit_str = limit.get().to_arraystring();
            params.push(("limit", &limit_str));
        }
        if let Some(guild_id) = guild_id {
            guild_id_str = guild_id.to_arraystring();
            params.push(("guild_id", &guild_id_str));
        }
        if let Some(exclude_ended) = exclude_ended {
            exclude_str = exclude_ended.to_arraystring();
            params.push(("exclude_ended", &exclude_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Entitlements {
                application_id: self.try_application_id()?,
            },
            params: Some(&params),
        })
        .await
    }

    /// Gets current gateway.
    pub async fn get_gateway(&self) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Gateway,
            params: None,
        })
        .await
    }

    /// Fetches all of the global commands for your application.
    pub async fn get_global_commands(&self) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Commands {
                application_id: self.try_application_id()?,
            },
            params: None,
        })
        .await
    }

    /// Fetches all of the global commands for your application with localizations.
    pub async fn get_global_commands_with_localizations(&self) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Commands {
                application_id: self.try_application_id()?,
            },
            params: Some(&[("with_localizations", "true")]),
        })
        .await
    }

    /// Fetches a global commands for your application by its Id.
    pub async fn get_global_command(&self, command_id: CommandId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Command {
                application_id: self.try_application_id()?,
                command_id,
            },
            params: None,
        })
        .await
    }

    /// Gets guild information.
    pub async fn get_guild(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Guild {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets guild information with counts.
    pub async fn get_guild_with_counts(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Guild {
                guild_id,
            },
            params: Some(&[("with_counts", "true")]),
        })
        .await
    }

    /// Fetches all of the guild commands for your application for a specific guild.
    pub async fn get_guild_commands(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildCommands {
                application_id: self.try_application_id()?,
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Fetches all of the guild commands with localizations for your application for a specific
    /// guild.
    pub async fn get_guild_commands_with_localizations(
        &self,
        guild_id: GuildId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildCommands {
                application_id: self.try_application_id()?,
                guild_id,
            },
            params: Some(&[("with_localizations", "true")]),
        })
        .await
    }

    /// Fetches a guild command by its Id.
    pub async fn get_guild_command(
        &self,
        guild_id: GuildId,
        command_id: CommandId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildCommand {
                application_id: self.try_application_id()?,
                guild_id,
                command_id,
            },
            params: None,
        })
        .await
    }

    /// Fetches all of the guild commands permissions for your application for a specific guild.
    pub async fn get_guild_commands_permissions(
        &self,
        guild_id: GuildId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildCommandsPermissions {
                application_id: self.try_application_id()?,
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gives the guild command permission for your application for a specific guild.
    pub async fn get_guild_command_permissions(
        &self,
        guild_id: GuildId,
        command_id: CommandId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildCommandPermissions {
                application_id: self.try_application_id()?,
                guild_id,
                command_id,
            },
            params: None,
        })
        .await
    }

    /// Gets a guild widget information.
    pub async fn get_guild_widget(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildWidget {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets a guild preview.
    pub async fn get_guild_preview(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildPreview {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets a guild welcome screen information.
    pub async fn get_guild_welcome_screen(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildWelcomeScreen {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets integrations that a guild has.
    pub async fn get_guild_integrations(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildIntegrations {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets all invites to a guild.
    pub async fn get_guild_invites(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildInvites {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets a guild's vanity URL if it has one.
    pub async fn get_guild_vanity_url(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildVanityUrl {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets the members of a guild. Optionally pass a `limit` and the Id of the user to offset the
    /// result by.
    pub async fn get_guild_members(
        &self,
        guild_id: GuildId,
        limit: Option<NonMaxU16>,
        after: Option<UserId>,
    ) -> ResultJson {
        let (limit_str, after_str);
        let mut params = ArrayVec::<_, 2>::new();

        limit_str = limit.unwrap_or(constants::MEMBER_FETCH_LIMIT).get().to_arraystring();
        params.push(("limit", limit_str.as_str()));

        if let Some(after) = after {
            after_str = after.to_arraystring();
            params.push(("after", &after_str));
        }

        let mut value: Value = self
            .fire(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::GuildMembers {
                    guild_id,
                },
                params: Some(&params),
            })
            .await?;

        if let Some(values) = value.as_array_mut() {
            for value in values {
                if let Some(element) = value.as_object_mut() {
                    element.insert("guild_id".to_string(), guild_id.get().into());
                }
            }
        }

        from_value(value).map_err(From::from)
    }

    /// Gets the amount of users that can be pruned.
    pub async fn get_guild_prune_count(&self, guild_id: GuildId, days: u8) -> ResultJson {
        let days_str = days.to_arraystring();
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildPrune {
                guild_id,
            },
            params: Some(&[("days", &days_str)]),
        })
        .await
    }

    /// Gets regions that a guild can use. If a guild has the `VIP_REGIONS` feature enabled, then
    /// additional VIP-only regions are returned.
    pub async fn get_guild_regions(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildRegions {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Retrieves a specific role in a [`Guild`].
    pub async fn get_guild_role(&self, guild_id: GuildId, role_id: RoleId) -> ResultJson {
        let mut value: Value = self
            .fire(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::GuildRole {
                    guild_id,
                    role_id,
                },
                params: None,
            })
            .await?;

        if let Some(map) = value.as_object_mut() {
            map.insert("guild_id".to_string(), guild_id.get().into());
        }

        from_value(value).map_err(From::from)
    }

    /// Retrieves a list of roles in a [`Guild`].
    pub async fn get_guild_roles(&self, guild_id: GuildId) -> ResultJson {
        let mut value: Value = self
            .fire(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::GuildRoles {
                    guild_id,
                },
                params: None,
            })
            .await?;

        if let Some(array) = value.as_array_mut() {
            for sticker in array {
                if let Some(map) = sticker.as_object_mut() {
                    map.insert("guild_id".to_string(), guild_id.get().into());
                }
            }
        }

        from_value(value).map_err(From::from)
    }

    /// Gets a scheduled event by Id.
    pub async fn get_scheduled_event(
        &self,
        guild_id: GuildId,
        event_id: ScheduledEventId,
        with_user_count: bool,
    ) -> ResultJson {
        let with_user_count_str = with_user_count.to_arraystring();
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildScheduledEvent {
                guild_id,
                event_id,
            },
            params: Some(&[("with_user_count", &with_user_count_str)]),
        })
        .await
    }

    /// Gets a list of all scheduled events for the corresponding guild.
    pub async fn get_scheduled_events(
        &self,
        guild_id: GuildId,
        with_user_count: bool,
    ) -> ResultJson {
        let with_user_count_str = with_user_count.to_arraystring();
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildScheduledEvents {
                guild_id,
            },
            params: Some(&[("with_user_count", &with_user_count_str)]),
        })
        .await
    }

    /// Gets a list of all interested users for the corresponding scheduled event, with additional
    /// options for filtering.
    pub async fn get_scheduled_event_users(
        &self,
        guild_id: GuildId,
        event_id: ScheduledEventId,
        limit: Option<NonMaxU8>,
        target: Option<UserPagination>,
        with_member: Option<bool>,
    ) -> ResultJson {
        let (limit_str, with_member_str, id_str);
        let mut params = ArrayVec::<_, 3>::new();
        if let Some(limit) = limit {
            limit_str = limit.get().to_arraystring();
            params.push(("limit", limit_str.as_str()));
        }
        if let Some(with_member) = with_member {
            with_member_str = with_member.to_arraystring();
            params.push(("with_member", &with_member_str));
        }
        if let Some(target) = target {
            let (name, id) = match target {
                UserPagination::After(id) => ("after", id),
                UserPagination::Before(id) => ("before", id),
            };

            id_str = id.to_arraystring();
            params.push((name, &id_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildScheduledEventUsers {
                guild_id,
                event_id,
            },
            params: Some(&params),
        })
        .await
    }

    /// Retrieves a list of stickers in a [`Guild`].
    pub async fn get_guild_stickers(&self, guild_id: GuildId) -> ResultJson {
        let mut value: Value = self
            .fire(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::GuildStickers {
                    guild_id,
                },
                params: None,
            })
            .await?;

        if let Some(array) = value.as_array_mut() {
            for role in array {
                if let Some(map) = role.as_object_mut() {
                    map.insert("guild_id".to_string(), guild_id.get().into());
                }
            }
        }

        from_value(value).map_err(From::from)
    }

    /// Retrieves a single sticker in a [`Guild`].
    pub async fn get_guild_sticker(
        &self,
        guild_id: GuildId,
        sticker_id: StickerId,
    ) -> ResultJson {
        let mut value: Value = self
            .fire(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::GuildSticker {
                    guild_id,
                    sticker_id,
                },
                params: None,
            })
            .await?;

        if let Some(map) = value.as_object_mut() {
            map.insert("guild_id".to_string(), guild_id.get().into());
        }

        from_value(value).map_err(From::from)
    }

    /// Retrieves the webhooks for the given [`Guild`]'s Id.
    pub async fn get_guild_webhooks(&self, guild_id: GuildId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::GuildWebhooks {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Gets a paginated list of the current user's guilds.
    ///
    /// The `limit` has a maximum value of 200.
    pub async fn get_guilds(
        &self,
        target: Option<GuildPagination>,
        limit: Option<NonMaxU8>,
    ) -> ResultJson {
        let (limit_str, id_str);
        let mut params = ArrayVec::<_, 2>::new();
        if let Some(limit) = limit {
            limit_str = limit.get().to_arraystring();
            params.push(("limit", limit_str.as_str()));
        }
        if let Some(target) = target {
            let (name, id) = match target {
                GuildPagination::After(id) => ("after", id),
                GuildPagination::Before(id) => ("before", id),
            };

            id_str = id.to_arraystring();
            params.push((name, &id_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::UserMeGuilds,
            params: Some(&params),
        })
        .await
    }

    /// Returns a guild [`Member`] object for the current user.
    ///
    /// This method only works for user tokens with the [`GuildsMembersRead`] OAuth2 scope.
    ///
    /// [`GuildsMembersRead`]: crate::model::application::Scope::GuildsMembersRead
    pub async fn get_current_user_guild_member(&self, guild_id: GuildId) -> ResultJson {
        let mut value: Value = self
            .fire(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::UserMeGuildMember {
                    guild_id,
                },
                params: None,
            })
            .await?;

        if let Some(map) = value.as_object_mut() {
            map.insert("guild_id".to_string(), guild_id.get().into());
        }

        from_value(value).map_err(From::from)
    }

    /// Gets information about a specific invite.
    pub async fn get_invite(
        &self,
        code: &str,
        member_counts: bool,
        expiration: bool,
        event_id: Option<ScheduledEventId>,
    ) -> ResultJson {
        let (member_counts_str, expiration_str, event_id_str);
        let mut params = ArrayVec::<_, 3>::new();

        member_counts_str = member_counts.to_arraystring();
        params.push(("with_counts", member_counts_str.as_str()));

        expiration_str = expiration.to_arraystring();
        params.push(("with_expiration", &expiration_str));

        if let Some(event_id) = event_id {
            event_id_str = event_id.to_arraystring();
            params.push(("guild_scheduled_event_id", &event_id_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Invite {
                code,
            },
            params: Some(&params),
        })
        .await
    }

    /// Gets member of a guild.
    pub async fn get_member(&self, guild_id: GuildId, user_id: UserId) -> ResultJson {
        let mut value: Value = self
            .fire(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::GuildMember {
                    guild_id,
                    user_id,
                },
                params: None,
            })
            .await?;

        if let Some(map) = value.as_object_mut() {
            map.insert("guild_id".to_string(), guild_id.get().into());
        }

        from_value(value).map_err(From::from)
    }

    /// Gets a message by an Id, bots only.
    pub async fn get_message(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::ChannelMessage {
                channel_id,
                message_id,
            },
            params: None,
        })
        .await
    }

    /// Gets X messages from a channel.
    pub async fn get_messages(
        &self,
        channel_id: GenericChannelId,
        target: Option<MessagePagination>,
        limit: Option<NonMaxU8>,
    ) -> ResultJson {
        let (limit_str, id_str);
        let mut params = ArrayVec::<_, 2>::new();

        if let Some(limit) = limit {
            limit_str = limit.get().to_arraystring();
            params.push(("limit", limit_str.as_str()));
        }

        if let Some(target) = target {
            let (name, id) = match target {
                MessagePagination::After(id) => ("after", id),
                MessagePagination::Around(id) => ("around", id),
                MessagePagination::Before(id) => ("before", id),
            };

            id_str = id.to_arraystring();
            params.push((name, &id_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::ChannelMessages {
                channel_id,
            },
            params: Some(&params),
        })
        .await
    }

    /// Retrieves a specific [`StickerPack`] from it's [`StickerPackId`]
    pub async fn get_sticker_pack(&self, sticker_pack_id: StickerPackId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::StickerPack {
                sticker_pack_id,
            },
            params: None,
        })
        .await
    }

    /// Retrieves a list of all nitro sticker packs.
    pub async fn get_nitro_stickers(&self) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::StickerPacks,
            params: None,
        })
        .await
    }

    /// Gets all pins of a channel.
    pub async fn get_pins(&self, channel_id: GenericChannelId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::ChannelPins {
                channel_id,
            },
            params: None,
        })
        .await
    }

    /// Gets user Ids based on their reaction to a message. This endpoint is dumb.
    pub async fn get_reaction_users(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
        reaction_type: &ReactionType,
        limit: u8,
        after: Option<UserId>,
    ) -> ResultJson {
        let (limit_str, after_str);
        let mut params = ArrayVec::<_, 2>::new();

        limit_str = limit.to_arraystring();
        params.push(("limit", limit_str.as_str()));

        if let Some(after) = after {
            after_str = after.to_arraystring();
            params.push(("after", &after_str));
        }

        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::ChannelMessageReactionEmoji {
                channel_id,
                message_id,
                reaction: &reaction_type.as_data(),
            },
            params: Some(&params),
        })
        .await
    }

    /// Gets all SKUs for the current application.
    pub async fn get_skus(&self) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Skus {
                application_id: self.try_application_id()?,
            },
            params: None,
        })
        .await
    }

    /// Gets a sticker.
    pub async fn get_sticker(&self, sticker_id: StickerId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Sticker {
                sticker_id,
            },
            params: None,
        })
        .await
    }

    /// Gets a user by Id.
    pub async fn get_user(&self, user_id: UserId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::User {
                user_id,
            },
            params: None,
        })
        .await
    }

    /// Gets the current user's third party connections.
    ///
    /// This method only works for user tokens with the [`Connections`] OAuth2 scope.
    ///
    /// [`Connections`]: crate::model::application::Scope::Connections
    pub async fn get_user_connections(&self) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::UserMeConnections,
            params: None,
        })
        .await
    }


    /// Gets all voice regions.
    pub async fn get_voice_regions(&self) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::VoiceRegions,
            params: None,
        })
        .await
    }

    /// Retrieves a webhook given its Id.
    ///
    /// This method requires authentication, whereas [`Http::get_webhook_with_token`] and
    /// [`Http::get_webhook_from_url`] do not.
    pub async fn get_webhook(&self, webhook_id: WebhookId) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::Webhook {
                webhook_id,
            },
            params: None,
        })
        .await
    }

    /// Retrieves a webhook given its Id and unique token.
    ///
    /// This method does _not_ require authentication.
    pub async fn get_webhook_with_token(
        &self,
        webhook_id: WebhookId,
        token: &str,
    ) -> ResultJson {
        self.fire(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Get,
            route: Route::WebhookWithToken {
                webhook_id,
                token,
            },
            params: None,
        })
        .await
    }

    /// Kicks a member from a guild with a provided reason.
    pub async fn kick_member(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::GuildMember {
                guild_id,
                user_id,
            },
            params: None,
        })
        .await
    }

    /// Leaves a guild.
    pub async fn leave_guild(&self, guild_id: GuildId) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Delete,
            route: Route::UserMeGuild {
                guild_id,
            },
            params: None,
        })
        .await
    }

    /// Sends a message to a channel.
    pub async fn send_message(
        &self,
        channel_id: GenericChannelId,
        files: Vec<CreateAttachment<'_>>,
        map: &impl serde::Serialize,
    ) -> ResultJson {
        let mut request = Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::ChannelMessages {
                channel_id,
            },
            params: None,
        };

        if files.is_empty() {
            request.body = Some(to_vec(map)?);
        } else {
            request.multipart = Some(Multipart {
                upload: MultipartUpload::Attachments(files),
                payload_json: Some(to_string(map)?),
                fields: vec![],
            });
        }

        self.fire(request).await
    }

    /// Pins a message in a channel.
    pub async fn pin_message(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Put,
            route: Route::ChannelPin {
                channel_id,
                message_id,
            },
            params: None,
        })
        .await
    }

    /// Unbans a user from a guild.
    pub async fn remove_ban(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::GuildBan {
                guild_id,
                user_id,
            },
            params: None,
        })
        .await
    }

    /// Deletes a single [`Role`] from a [`Member`] in a [`Guild`].
    pub async fn remove_member_role(
        &self,
        guild_id: GuildId,
        user_id: UserId,
        role_id: RoleId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::GuildMemberRole {
                guild_id,
                user_id,
                role_id,
            },
            params: None,
        })
        .await
    }

    /// Returns a list of [`Member`]s in a [`Guild`] whose username or nickname starts with a
    /// provided string.
    pub async fn search_guild_members(
        &self,
        guild_id: GuildId,
        query: &str,
        limit: Option<NonMaxU16>,
    ) -> ResultJson {
        let limit_str = limit.unwrap_or(constants::MEMBER_FETCH_LIMIT).get().to_arraystring();
        let mut value: Value = self
            .fire(Request {
                body: None,
                multipart: None,
                headers: None,
                method: LightMethod::Get,
                route: Route::GuildMembersSearch {
                    guild_id,
                },
                params: Some(&[("query", query), ("limit", &limit_str)]),
            })
            .await?;

        if let Some(members) = value.as_array_mut() {
            for member in members {
                if let Some(map) = member.as_object_mut() {
                    map.insert("guild_id".to_string(), guild_id.get().into());
                }
            }
        }

        from_value(value).map_err(From::from)
    }

    /// Starts removing some members from a guild based on the last time they've been online.
    pub async fn start_guild_prune(
        &self,
        guild_id: GuildId,
        days: u8,
        audit_log_reason: Option<&str>,
    ) -> ResultJson {
        let days_str = days.to_arraystring();
        self.fire(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Post,
            route: Route::GuildPrune {
                guild_id,
            },
            params: Some(&[("days", &days_str)]),
        })
        .await
    }

    /// Starts syncing an integration with a guild.
    pub async fn start_integration_sync(
        &self,
        guild_id: GuildId,
        integration_id: IntegrationId,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: None,
            method: LightMethod::Post,
            route: Route::GuildIntegrationSync {
                guild_id,
                integration_id,
            },
            params: None,
        })
        .await
    }

    /// Unpins a message from a channel.
    pub async fn unpin_message(
        &self,
        channel_id: GenericChannelId,
        message_id: MessageId,
        audit_log_reason: Option<&str>,
    ) -> Result<()> {
        self.wind(Request {
            body: None,
            multipart: None,
            headers: audit_log_reason.map(reason_into_header),
            method: LightMethod::Delete,
            route: Route::ChannelPin {
                channel_id,
                message_id,
            },
            params: None,
        })
        .await
    }

    /// Fires off a request, deserializing the response reader via the given type bound.
    ///
    /// If you don't need to deserialize the response and want the response instance itself, use
    /// [`Self::request`].
    ///
    /// # Errors
    ///
    /// If there is an error, it will be either [`Error::Http`] or [`Error::Json`].
    pub async fn fire<T: DeserializeOwned>(&self, req: Request<'_>) -> Result<T> {
        let response = self.request(req).await?;
        let response_de = response.json().await?;
        Ok(response_de)
    }

    /// Performs a request, ratelimiting it if necessary.
    ///
    /// Returns the raw reqwest Response. Use [`Self::fire`] to deserialize the response into some
    /// type.
    #[cfg_attr(feature = "tracing_instrument", instrument)]
    pub async fn request(&self, req: Request<'_>) -> Result<ReqwestResponse> {
        self.http.request(req).await
    }

    /// Performs a request and verifies that Discord responds with [`StatusCode::NO_CONTENT`].
    ///
    /// This is a function that performs a light amount of work and returns the unit type, so it's
    /// called "self.wind" to denote that it's lightweight.
    pub(super) async fn wind(&self, req: Request<'_>) -> Result<()> {
        let route = req.route;
        let method = req.method.reqwest_method();
        let response = self.request(req).await?;

        let status = response.status();
        if status.is_success() {
            if status != StatusCode::NO_CONTENT {
                let route = route.path();
                warn!(
                    "Mismatched successful response status from {route}! Expected 'No Content' but got {status}"
                );
            }

            return Ok(());
        }

        debug!("Unsuccessful response: {response:?}");
        Err(Error::Http(HttpError::UnsuccessfulRequest(
            ErrorResponse::from_response(response, method).await,
        )))
    }
}
