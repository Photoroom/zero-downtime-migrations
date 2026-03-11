# Concurrent AddIndex - should pass
from django.db import migrations
from django.contrib.postgres.operations import AddIndexConcurrently


class Migration(migrations.Migration):

    atomic = False

    dependencies = [
        ('myapp', '0001_initial'),
    ]

    operations = [
        AddIndexConcurrently(
            model_name='order',
            index=models.Index(fields=['created_at'], name='order_created_idx'),
        ),
    ]
